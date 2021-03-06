use crate::{
    config::{Configuration, InterfaceConfiguration, InterfaceDriver, StorageConfiguration},
    context_builder::ContextBuilder,
    error::HolochainInstanceError,
    logger::DebugLogger,
    Holochain,
};
use holochain_core::{
    logger::{ChannelLogger, Logger},
    signal::Signal,
};
use holochain_core_types::{
    agent::{AgentId, KeyBuffer},
    dna::Dna,
    error::HolochainError,
    json::JsonString,
};
use jsonrpc_ws_server::jsonrpc_core::IoHandler;

use std::{
    clone::Clone,
    collections::HashMap,
    convert::TryFrom,
    fs::File,
    io::prelude::*,
    sync::{mpsc::SyncSender, Arc, Mutex, RwLock},
    thread,
};

use holochain_net::p2p_config::P2pConfig;
use holochain_net_connection::net_connection::NetShutdown;
use holochain_net_ipc::spawn::{ipc_spawn, SpawnResult};
use interface::{ContainerApiBuilder, InstanceMap, Interface};
/// Main representation of the container.
/// Holds a `HashMap` of Holochain instances referenced by ID.

/// A primary point in this struct is
/// `load_config(&mut self, config: &Configuration) -> Result<(), String>`
/// which takes a `config::Configuration` struct and tries to instantiate all configured instances.
/// While doing so it has to load DNA files referenced in the configuration.
/// In order to not bind this code to the assumption that there is a filesystem
/// and also enable easier testing, a DnaLoader ()which is a closure that returns a
/// Dna object for a given path string) has to be injected on creation.
pub struct Container {
    instances: InstanceMap,
    config: Configuration,
    interface_threads: HashMap<String, InterfaceThreadHandle>,
    dna_loader: DnaLoader,
    signal_tx: Option<SignalSender>,
    logger: DebugLogger,
    p2p_config: Option<JsonString>,
    network_child_process: NetShutdown,
}

impl Drop for Container {
    fn drop(&mut self) {
        if let Some(kill) = self.network_child_process.take() {
            kill();
        }
    }
}

type SignalSender = SyncSender<Signal>;
type InterfaceThreadHandle = thread::JoinHandle<Result<(), String>>;
type DnaLoader = Arc<Box<FnMut(&String) -> Result<Dna, HolochainError> + Send>>;

// preparing for having container notifiers go to one of the log streams
pub fn notify(msg: String) {
    println!("{}", msg);
}

impl Container {
    /// Creates a new instance with the default DnaLoader that actually loads files.
    pub fn from_config(config: Configuration) -> Self {
        let rules = config.logger.rules.clone();
        Container {
            instances: HashMap::new(),
            interface_threads: HashMap::new(),
            config,
            dna_loader: Arc::new(Box::new(Self::load_dna)),
            signal_tx: None,
            logger: DebugLogger::new(rules),
            p2p_config: None,
            network_child_process: None,
        }
    }

    pub fn with_signal_channel(mut self, signal_tx: SyncSender<Signal>) -> Self {
        if !self.instances.is_empty() {
            panic!("Cannot set a signal channel after having run load_config()");
        }
        self.signal_tx = Some(signal_tx);
        self
    }

    pub fn config(&self) -> Configuration {
        self.config.clone()
    }

    pub fn start_all_interfaces(&mut self) {
        self.interface_threads = self
            .config
            .interfaces
            .iter()
            .map(|ic| (ic.id.clone(), self.spawn_interface_thread(ic.clone())))
            .collect()
    }

    pub fn start_interface_by_id(&mut self, id: String) -> Result<(), String> {
        self.config
            .interface_by_id(&id)
            .ok_or(format!("Interface does not exist: {}", id))
            .and_then(|config| self.start_interface(&config))
    }

    /// Starts all instances
    pub fn start_all_instances(&mut self) -> Result<(), HolochainInstanceError> {
        self.instances
            .iter_mut()
            .map(|(id, hc)| {
                notify(format!("Starting instance \"{}\"...", id));
                hc.write().unwrap().start()
            })
            .collect::<Result<Vec<()>, _>>()
            .map(|_| ())
    }

    /// Stops all instances
    pub fn stop_all_instances(&mut self) -> Result<(), HolochainInstanceError> {
        self.instances
            .iter_mut()
            .map(|(id, hc)| {
                notify(format!("Stopping instance \"{}\"...", id));
                hc.write().unwrap().stop()
            })
            .collect::<Result<Vec<()>, _>>()
            .map(|_| ())
    }

    pub fn instances(&self) -> &InstanceMap {
        &self.instances
    }

    /// Stop and clear all instances
    pub fn shutdown(&mut self) -> Result<(), HolochainInstanceError> {
        self.stop_all_instances()?;
        // @TODO: also stop all interfaces
        self.instances = HashMap::new();
        Ok(())
    }

    pub fn spawn_network(&mut self) -> Result<String, HolochainError> {
        let network_config = self
            .config
            .clone()
            .network
            .ok_or(HolochainError::ErrorGeneric(
                "attempt to spawn network when not configured".to_string(),
            ))?;

        println!(
            "Spawning network with working directory: {}",
            network_config.n3h_persistence_path
        );
        let SpawnResult {
            kill,
            ipc_binding,
            p2p_bindings: _,
        } = ipc_spawn(
            "node".to_string(),
            vec![format!(
                "{}/packages/n3h/bin/n3h",
                network_config.n3h_path.clone()
            )],
            network_config.n3h_persistence_path.clone(),
            hashmap! {
                String::from("N3H_MODE") => network_config.n3h_mode.clone(),
                String::from("N3H_WORK_DIR") => network_config.n3h_persistence_path.clone(),
                String::from("N3H_IPC_SOCKET") => String::from("tcp://127.0.0.1:*"),
            },
            true,
        )
        .map_err(|error| {
            println!("Error spawning network process! {:?}", error);
            HolochainError::ErrorGeneric(error.to_string())
        })?;
        self.network_child_process = kill;
        println!("Network spawned with binding: {:?}", ipc_binding);
        Ok(ipc_binding)
    }

    fn instance_p2p_config(&self) -> Result<JsonString, HolochainError> {
        let config = self.p2p_config.clone().unwrap_or_else(|| {
            // This should never happen, but we'll throw out a named mock network rather than crashing,
            // just to be nice (TODO make proper logging statement)
            println!("warn: instance_network_config called before p2p_config initialized! Using default mock network name.");
            JsonString::from(P2pConfig::named_mock_config("container-default-mock"))
        });
        Ok(config)
    }

    fn initialize_p2p_config(&mut self) -> JsonString {
        match self.config.network.clone() {
            // if there is a config then either we need to spawn a process and get the
            // ipc_uri for it and save it for future calls to `load_config`
            // or we use that uri value that was created from previous calls!
            Some(ref net_config) => {
                let uri = self
                    .config
                    .clone()
                    .network
                    .unwrap() // unwrap safe because of check above
                    .n3h_ipc_uri
                    .clone()
                    .or_else(|| self.spawn_network().ok());
                JsonString::from(json!(
                    {
                        "backend_kind": "IPC",
                        "backend_config": {
                            "socketType": "zmq",
                            "bootstrapNodes": net_config.bootstrap_nodes,
                            "ipcUri": uri
                        }
                    }
                ))
            }
            // if there's no NetworkConfig we won't spawn a network process
            // and instead configure instances to use a unique mock network
            None => JsonString::from(P2pConfig::unique_mock_config()),
        }
    }

    /// Tries to create all instances configured in the given Configuration object.
    /// Calls `Configuration::check_consistency()` first and clears `self.instances`.
    /// The first time we call this, we also initialize the container-wide config
    /// for use with all instances
    ///
    /// @TODO: clean up the container creation process to prevent loading config before proper setup,
    ///        especially regarding the signal handler.
    ///        (see https://github.com/holochain/holochain-rust/issues/739)
    pub fn load_config(&mut self) -> Result<(), String> {
        let _ = self.config.check_consistency()?;

        if self.p2p_config.is_none() {
            self.p2p_config = Some(self.initialize_p2p_config());
        }

        let config = self.config.clone();
        self.shutdown().map_err(|e| e.to_string())?;
        self.instances = HashMap::new();

        for id in config.instance_ids_sorted_by_bridge_dependencies()? {
            let instance = self
                .instantiate_from_config(&id, &config)
                .map_err(|error| {
                    format!(
                        "Error while trying to create instance \"{}\": {}",
                        id, error
                    )
                })?;

            self.instances
                .insert(id.clone(), Arc::new(RwLock::new(instance)));
        }
        Ok(())
    }

    /// Creates one specific Holochain instance from a given Configuration,
    /// id string and DnaLoader.
    pub fn instantiate_from_config(
        &mut self,
        id: &String,
        config: &Configuration,
    ) -> Result<Holochain, String> {
        let _ = config.check_consistency()?;

        config
            .instance_by_id(&id)
            .ok_or(String::from("Instance not found in config"))
            .and_then(|instance_config| {
                // Build context:
                let mut context_builder = ContextBuilder::new();

                // Agent:
                let agent_config = config.agent_by_id(&instance_config.agent).unwrap();
                let pub_key = KeyBuffer::with_corrected(&agent_config.public_address)?;
                context_builder =
                    context_builder.with_agent(AgentId::new(&agent_config.name, &pub_key));

                context_builder = context_builder.with_network_config(self.instance_p2p_config()?);

                // Storage:
                if let StorageConfiguration::File { path } = instance_config.storage {
                    context_builder = context_builder.with_file_storage(path).map_err(|hc_err| {
                        format!("Error creating context: {}", hc_err.to_string())
                    })?
                };

                if config.logger.logger_type == "debug" {
                    context_builder = context_builder.with_logger(Arc::new(Mutex::new(
                        ChannelLogger::new(instance_config.id.clone(), self.logger.get_sender()),
                    )));
                }

                // Container API
                let mut api_builder = ContainerApiBuilder::new();
                // Bridges:
                let id = instance_config.id.clone();
                for bridge in config.bridge_dependencies(id.clone()) {
                    assert_eq!(bridge.caller_id, id.clone());
                    let callee_config = config
                        .instance_by_id(&bridge.callee_id)
                        .expect("config.check_consistency()? jumps out if config is broken");
                    let callee_instance = self.instances.get(&bridge.callee_id).expect(
                        r#"
                            We have to create instances ordered by bridge dependencies such that we
                            can expect the callee to be present here because we need it to create
                            the bridge API"#,
                    );

                    api_builder = api_builder
                        .with_named_instance(bridge.handle.clone(), callee_instance.clone());
                    api_builder = api_builder
                        .with_named_instance_config(bridge.handle.clone(), callee_config);
                }
                context_builder = context_builder.with_container_api(api_builder.spawn());
                if let Some(signal_tx) = self.signal_tx.clone() {
                    context_builder = context_builder.with_signals(signal_tx);
                }

                // Spawn context
                let context = context_builder.spawn();

                // Get DNA
                let dna_config = config.dna_by_id(&instance_config.dna).unwrap();
                let dna = Arc::get_mut(&mut self.dna_loader).unwrap()(&dna_config.file).map_err(
                    |_| {
                        HolochainError::ConfigError(format!(
                            "Could not load DNA file \"{}\"",
                            dna_config.file
                        ))
                    },
                )?;

                Holochain::new(dna, Arc::new(context)).map_err(|hc_err| hc_err.to_string())
            })
    }

    fn start_interface(&mut self, config: &InterfaceConfiguration) -> Result<(), String> {
        if self.interface_threads.contains_key(&config.id) {
            return Err(format!("Interface {} already started!", config.id));
        }
        let handle = self.spawn_interface_thread(config.clone());
        self.interface_threads.insert(config.id.clone(), handle);
        Ok(())
    }

    /// Default DnaLoader that actually reads files from the filesystem
    fn load_dna(file: &String) -> Result<Dna, HolochainError> {
        let mut f = File::open(file)?;
        let mut contents = String::new();
        f.read_to_string(&mut contents)?;
        Dna::try_from(JsonString::from(contents))
    }

    fn make_interface_handler(&self, interface_config: &InterfaceConfiguration) -> IoHandler {
        let instance_ids: Vec<String> = interface_config
            .instances
            .iter()
            .map(|i| i.id.clone())
            .collect();

        let instance_subset: InstanceMap = self
            .instances
            .iter()
            .filter(|(id, _)| instance_ids.contains(&id))
            .map(|(id, val)| (id.clone(), val.clone()))
            .collect();

        ContainerApiBuilder::new()
            .with_instances(instance_subset)
            .with_instance_configs(self.config.instances.clone())
            .spawn()
    }

    fn spawn_interface_thread(
        &self,
        interface_config: InterfaceConfiguration,
    ) -> InterfaceThreadHandle {
        let dispatcher = self.make_interface_handler(&interface_config);
        let log_sender = self.logger.get_sender();
        thread::spawn(move || {
            let iface = make_interface(&interface_config);
            iface.run(dispatcher).map_err(|error| {
                let message = format!(
                    "err/container: Error running interface '{}': {}",
                    interface_config.id, error
                );
                let _ = log_sender.send((String::from("container"), message));
                error
            })
        })
    }
}

impl<'a> TryFrom<&'a Configuration> for Container {
    type Error = HolochainError;
    fn try_from(config: &'a Configuration) -> Result<Self, Self::Error> {
        let mut container = Container::from_config((*config).clone());
        container
            .load_config()
            .map_err(|string| HolochainError::ConfigError(string))?;
        Ok(container)
    }
}

/// This can eventually be dependency injected for third party Interface definitions
fn make_interface(interface_config: &InterfaceConfiguration) -> Box<Interface> {
    use interface_impls::{http::HttpInterface, websocket::WebsocketInterface};
    match interface_config.driver {
        InterfaceDriver::Websocket { port } => Box::new(WebsocketInterface::new(port)),
        InterfaceDriver::Http { port } => Box::new(HttpInterface::new(port)),
        _ => unimplemented!(),
    }
}

#[derive(Clone, Debug)]
struct NullLogger {}

impl Logger for NullLogger {
    fn log(&mut self, _msg: String) {}
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::config::load_configuration;
    use holochain_core::{action::Action, signal::signal_channel};
    use holochain_core_types::{cas::content::Address, dna, json::RawString};
    use holochain_wasm_utils::wasm_target_dir;
    use std::{fs::File, io::Write};
    use tempfile::tempdir;
    use test_utils::*;

    pub fn test_dna_loader() -> DnaLoader {
        let loader = Box::new(|path: &String| {
            Ok(match path.as_ref() {
                "bridge/callee.dna" => callee_dna(),
                "bridge/caller.dna" => caller_dna(),
                _ => Dna::try_from(JsonString::from(example_dna_string())).unwrap(),
            })
        }) as Box<FnMut(&String) -> Result<Dna, HolochainError> + Send>;
        Arc::new(loader)
    }

    pub fn test_toml() -> String {
        r#"
    [[agents]]
    id = "test-agent-1"
    name = "Holo Tester 1"
    public_address = "HoloTester1-----------------------------------------------------------------------AAACZp4xHB"
    key_file = "holo_tester.key"

    [[agents]]
    id = "test-agent-2"
    name = "Holo Tester 2"
    public_address = "HoloTester2-----------------------------------------------------------------------AAAGy4WW9e"
    key_file = "holo_tester.key"

    [[agents]]
    id = "test-agent-3"
    name = "Holo Tester 3"
    public_address = "HoloTester2-----------------------------------------------------------------------AAAGy4WW9e"
    key_file = "holo_tester.key"

    [[dnas]]
    id = "test-dna"
    file = "app_spec.hcpkg"
    hash = "Qm328wyq38924y"

    [[dnas]]
    id = "bridge-callee"
    file = "bridge/callee.dna"
    hash = "Qm328wyq38924y"

    [[dnas]]
    id = "bridge-caller"
    file = "bridge/caller.dna"
    hash = "Qm328wyq38924y"

    [[instances]]
    id = "test-instance-1"
    dna = "bridge-callee"
    agent = "test-agent-1"
    [instances.storage]
    type = "memory"

    [[instances]]
    id = "test-instance-2"
    dna = "test-dna"
    agent = "test-agent-2"
    [instances.storage]
    type = "memory"

    [[instances]]
    id = "bridge-caller"
    dna = "bridge-caller"
    agent = "test-agent-3"
    [instances.storage]
    type = "memory"

    [[interfaces]]
    id = "test-interface"
    [interfaces.driver]
    type = "websocket"
    port = 8888
    [[interfaces.instances]]
    id = "test-instance-1"
    [[interfaces.instances]]
    id = "test-instance-2"

    [[interfaces]]
    id = "test-interface"
    [interfaces.driver]
    type = "http"
    port = 4000
    [[interfaces.instances]]
    id = "test-instance-1"
    [[interfaces.instances]]
    id = "test-instance-2"

    [[bridges]]
    caller_id = "test-instance-2"
    callee_id = "test-instance-1"
    handle = "DPKI"

    [[bridges]]
    caller_id = "bridge-caller"
    callee_id = "test-instance-2"
    handle = "happ-store"

    [[bridges]]
    caller_id = "bridge-caller"
    callee_id = "test-instance-1"
    handle = "test-callee"
    "#
        .to_string()
    }

    pub fn test_container() -> Container {
        let config = load_configuration::<Configuration>(&test_toml()).unwrap();
        let mut container = Container::from_config(config.clone());
        container.dna_loader = test_dna_loader();
        container.load_config().unwrap();
        container
    }

    fn test_container_with_signals(signal_tx: SignalSender) -> Container {
        let config = load_configuration::<Configuration>(&test_toml()).unwrap();
        let mut container = Container::from_config(config.clone()).with_signal_channel(signal_tx);
        container.dna_loader = test_dna_loader();
        container.load_config().unwrap();
        container
    }

    pub fn example_dna_string() -> String {
        r#"{
                "name": "my dna",
                "description": "",
                "version": "",
                "uuid": "00000000-0000-0000-0000-000000000001",
                "dna_spec_version": "2.0",
                "properties": {},
                "zomes": {
                    "": {
                        "description": "",
                        "config": {
                            "error_handling": "throw-errors"
                        },
                        "entry_types": {
                            "": {
                                "description": "",
                                "sharing": "public"
                            }
                        },
                        "capabilities": {
                            "test": {
                                "type": "public",
                                "functions": [
                                    {
                                        "name": "test",
                       "inputs" : [
                            {
                                "name": "post",
                                "type": "string"
                            }
                        ],
                        "outputs" : [
                            {
                                "name": "hash",
                                "type": "string"
                            }
                        ]
                                    }
                                ]
                            }
                        },
                        "code": {
                            "code": "AAECAw=="
                        }
                    }
                }
            }"#
        .to_string()
    }

    #[test]
    fn test_default_dna_loader() {
        let tempdir = tempdir().unwrap();
        let file_path = tempdir.path().join("test.dna.json");
        let mut tmp_file = File::create(file_path.clone()).unwrap();
        writeln!(tmp_file, "{}", example_dna_string()).unwrap();
        match Container::load_dna(&file_path.into_os_string().into_string().unwrap()) {
            Ok(dna) => {
                assert_eq!(dna.name, "my dna");
            }
            Err(_) => assert!(false),
        }
    }

    #[test]
    fn test_container_load_config() {
        let mut container = test_container();
        assert_eq!(container.instances.len(), 3);

        container.start_all_instances().unwrap();
        container.start_all_interfaces();
        container.stop_all_instances().unwrap();
    }

    #[test]
    fn test_container_try_from_configuration() {
        let config = load_configuration::<Configuration>(&test_toml()).unwrap();

        let maybe_container = Container::try_from(&config);

        assert!(maybe_container.is_err());
        assert_eq!(
            maybe_container.err().unwrap(),
            HolochainError::ConfigError(
                "Error while trying to create instance \"test-instance-1\": Could not load DNA file \"bridge/callee.dna\"".to_string()
            )
        );
    }

    #[test]
    fn test_rpc_info_instances() {
        let container = test_container();
        let interface_config = &container.config.interfaces[0];
        let io = container.make_interface_handler(&interface_config);

        let request = r#"{"jsonrpc": "2.0", "method": "info/instances", "params": null, "id": 1}"#;
        let response = io
            .handle_request_sync(request)
            .expect("No response returned for info/instances");
        assert!(response.contains("test-instance-1"));
        assert!(response.contains("test-instance-2"));
    }

    #[test]
    fn test_container_signal_handler() {
        let (signal_tx, signal_rx) = signal_channel();
        let _container = test_container_with_signals(signal_tx);

        test_utils::expect_action(&signal_rx, |action| match action {
            Action::InitApplication(_) => true,
            _ => false,
        })
        .unwrap();

        // expect one InitNetwork for each instance

        test_utils::expect_action(&signal_rx, |action| match action {
            Action::InitNetwork(_) => true,
            _ => false,
        })
        .unwrap();

        test_utils::expect_action(&signal_rx, |action| match action {
            Action::InitNetwork(_) => true,
            _ => false,
        })
        .unwrap();
    }

    pub fn callee_wat() -> String {
        r#"
(module

    (memory 1)
    (export "memory" (memory 0))

    (func
        (export "__hdk_validate_app_entry")
        (param $allocation i32)
        (result i32)

        (i32.const 0)
    )

    (func
        (export "__hdk_validate_link")
        (param $allocation i32)
        (result i32)

        (i32.const 0)
    )


    (func
        (export "__hdk_get_validation_package_for_entry_type")
        (param $allocation i32)
        (result i32)

        ;; This writes "Entry" into memory
        (i32.store (i32.const 0) (i32.const 34))
        (i32.store (i32.const 1) (i32.const 69))
        (i32.store (i32.const 2) (i32.const 110))
        (i32.store (i32.const 3) (i32.const 116))
        (i32.store (i32.const 4) (i32.const 114))
        (i32.store (i32.const 5) (i32.const 121))
        (i32.store (i32.const 6) (i32.const 34))

        (i32.const 7)
    )

    (func
        (export "__hdk_get_validation_package_for_link")
        (param $allocation i32)
        (result i32)

        ;; This writes "Entry" into memory
        (i32.store (i32.const 0) (i32.const 34))
        (i32.store (i32.const 1) (i32.const 69))
        (i32.store (i32.const 2) (i32.const 110))
        (i32.store (i32.const 3) (i32.const 116))
        (i32.store (i32.const 4) (i32.const 114))
        (i32.store (i32.const 5) (i32.const 121))
        (i32.store (i32.const 6) (i32.const 34))

        (i32.const 7)
    )

    (func
        (export "__list_capabilities")
        (param $allocation i32)
        (result i32)

        (i32.const 0)
    )

    (func
        (export "hello")
        (param $allocation i32)
        (result i32)

        ;; This writes "Holo World" into memory
        (i32.store (i32.const 0) (i32.const 72))
        (i32.store (i32.const 1) (i32.const 111))
        (i32.store (i32.const 2) (i32.const 108))
        (i32.store (i32.const 3) (i32.const 111))
        (i32.store (i32.const 4) (i32.const 32))
        (i32.store (i32.const 5) (i32.const 87))
        (i32.store (i32.const 6) (i32.const 111))
        (i32.store (i32.const 7) (i32.const 114))
        (i32.store (i32.const 8) (i32.const 108))
        (i32.store (i32.const 9) (i32.const 100))

        (i32.const 10)
    )
)
                "#
        .to_string()
    }

    fn callee_dna() -> Dna {
        let wat = &callee_wat();
        let mut dna = create_test_dna_with_wat("greeter", "public", Some(wat));
        dna.uuid = String::from("basic_bridge_call");
        dna.zomes
            .get_mut("greeter")
            .unwrap()
            .capabilities
            .get_mut("public")
            .unwrap()
            .functions
            .push(dna::capabilities::FnDeclaration {
                name: String::from("hello"),
                inputs: vec![],
                outputs: vec![dna::capabilities::FnParameter {
                    name: String::from("greeting"),
                    parameter_type: String::from("String"),
                }],
            });
        dna
    }

    fn caller_dna() -> Dna {
        let wasm = create_wasm_from_file(&format!(
            "{}/wasm32-unknown-unknown/release/test_bridge_caller.wasm",
            wasm_target_dir("container_api/", "test-bridge-caller/"),
        ));
        let capabability = create_test_cap_with_fn_name("call_bridge");
        let mut dna = create_test_dna_with_cap("main", "main", &capabability, &wasm);
        dna.uuid = String::from("basic_bridge_call");
        dna
    }

    #[test]
    fn basic_bridge_call_roundtrip() {
        let config = load_configuration::<Configuration>(&test_toml()).unwrap();
        let mut container = Container::from_config(config.clone());
        container.dna_loader = test_dna_loader();
        container.load_config().expect("Test config must be sane");
        container
            .start_all_instances()
            .expect("Instances must be spawnable");
        let caller_instance = container.instances["bridge-caller"].clone();
        let result = caller_instance
            .write()
            .unwrap()
            .call(
                "main",
                Some(dna::capabilities::CapabilityCall::new(
                    String::from("main"),
                    Address::from("fake_token"),
                    None,
                )),
                "call_bridge",
                "{}",
            )
            .unwrap();

        // "Holo World" comes for the callee_wat above which runs in the callee instance
        assert_eq!(result, JsonString::from(RawString::from("Holo World")));
    }

}
