use crate::benchmarker::Mode;
use crate::config::{Named, Project, Test};
use crate::docker::docker_config::DockerConfig;
use crate::docker::listener::application::Application;
use crate::docker::listener::benchmark_command_listener::BenchmarkCommandListener;
use crate::docker::listener::benchmarker::{BenchmarkResults, Benchmarker};
use crate::docker::listener::build_container::BuildContainer;
use crate::docker::listener::simple::Simple;
use crate::docker::listener::verifier::Verifier;
use crate::docker::{
    BenchmarkCommands, DockerContainerIdFuture, DockerOrchestration, Verification,
};
use crate::error::ToolsetError::{
    ContainerPortMappingInspectionError, ExposePortError, FailedBenchmarkCommandRetrievalError,
};
use crate::error::ToolsetResult;
use crate::io::Logger;
use dockurl::container::create::host_config::{HostConfig, Ulimit};
use dockurl::container::create::networking_config::{
    EndpointSettings, EndpointsConfig, NetworkingConfig,
};
use dockurl::container::create::options::Options;
use dockurl::container::{
    attach_to_container, delete_container, get_container_logs, inspect_container, kill_container,
    wait_for_container_to_exit,
};
use dockurl::image::{delete_image, delete_unused_images};
use dockurl::network::NetworkMode;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::thread;
use std::time::Duration;

/// Note: this function makes the assumption that the image is already
/// built and that the Docker daemon is aware of it.
pub fn create_container(
    config: &DockerConfig,
    image_id: &str,
    network_id: &str,
    host_name: &str,
    docker_host: &str,
) -> ToolsetResult<String> {
    let mut options = Options::new();
    options.image(image_id);
    options.hostname(host_name);
    options.domain_name(host_name);

    let mut host_config = HostConfig::new();
    let mut endpoint_settings = EndpointSettings::new();
    endpoint_settings.network_id(network_id);
    match &config.network_mode {
        dockurl::network::NetworkMode::Bridge => {
            host_config.network_mode(dockurl::network::NetworkMode::Bridge);
            endpoint_settings.alias(host_name);
        }
        dockurl::network::NetworkMode::Host => {
            host_config.extra_host("tfb-database", &config.database_host);
            host_config.network_mode(dockurl::network::NetworkMode::Host);
        }
    }
    let mut sysctls = HashMap::new();
    sysctls.insert("net.core.somaxconn", "65535");
    host_config.sysctls(sysctls);
    host_config.ulimits(vec![
        Ulimit {
            name: "nofile",
            soft: 200000,
            hard: 200000,
        },
        Ulimit {
            name: "rtprio",
            soft: 99,
            hard: 99,
        },
    ]);
    host_config.publish_all_ports(true);
    host_config.privileged(true);

    options.networking_config(NetworkingConfig {
        endpoints_config: EndpointsConfig { endpoint_settings },
    });

    options.host_config(host_config);
    options.tty(true);

    let container_id = dockurl::container::create_container(
        options,
        config.use_unix_socket,
        docker_host,
        BuildContainer::new(),
    )?;

    Ok(container_id)
}

/// Creates the benchmarker container and returns the Docker ID
pub fn create_benchmarker_container(
    config: &DockerConfig,
    command_strs: &[String],
) -> ToolsetResult<String> {
    let mut options = Options::new();
    options.image("techempower/tfb.verifier");
    options.tty(true);
    options.attach_stderr(true);
    // The command_str we get back is an array of strings that make up the wrk
    // command; we want to replace `tfb-server` with the IP address
    let mut command = vec![];
    for command_str in command_strs {
        command.push(command_str.replace("tfb-server", &config.server_host));
    }
    options.cmds(command.as_slice());

    let mut host_config = HostConfig::new();
    match &config.network_mode {
        dockurl::network::NetworkMode::Bridge => {
            host_config.network_mode(dockurl::network::NetworkMode::Bridge);
        }
        dockurl::network::NetworkMode::Host => {
            host_config.extra_host("tfb-server", &config.server_host);
            host_config.network_mode(dockurl::network::NetworkMode::Host);
        }
    }
    let mut sysctls = HashMap::new();
    sysctls.insert("net.core.somaxconn", "65535");
    host_config.sysctls(sysctls);
    let ulimit = Ulimit {
        name: "nofile",
        soft: 65535,
        hard: 65535,
    };
    host_config.ulimits(vec![ulimit]);

    options.host_config(host_config);

    let mut endpoint_settings = EndpointSettings::new();
    endpoint_settings.network_id(config.client_network_id.as_str());

    options.networking_config(NetworkingConfig {
        endpoints_config: EndpointsConfig { endpoint_settings },
    });

    let container_id = dockurl::container::create_container(
        options,
        config.use_unix_socket,
        &config.client_docker_host,
        BuildContainer::new(),
    )?;

    Ok(container_id)
}

/// Creates the container for the `TFBVerifier`.
/// Note: this function makes the assumption that the image has already been
/// pulled from Dockerhub and the Docker daemon is aware of it.
pub fn create_verifier_container(
    config: &DockerConfig,
    orchestration: &DockerOrchestration,
    mode: Mode,
    test_type: &(&String, &String),
) -> ToolsetResult<String> {
    let mut options = Options::new();
    options.image("techempower/tfb.verifier");
    options.tty(true);
    options.add_env(
        "MODE",
        match mode {
            Mode::Verify => "verify",
            Mode::Benchmark => "benchmark",
        },
    );
    options.add_env("PORT", &orchestration.host_internal_port);
    options.add_env("ENDPOINT", test_type.1);
    options.add_env("TEST_TYPE", test_type.0);
    options.add_env("CONCURRENCY_LEVELS", &config.concurrency_levels);
    options.add_env(
        "PIPELINE_CONCURRENCY_LEVELS",
        &config.pipeline_concurrency_levels,
    );
    if let Some(database_name) = &orchestration.database_name {
        options.add_env("DATABASE", database_name);
    }

    let mut host_config = HostConfig::new();
    match &config.network_mode {
        dockurl::network::NetworkMode::Bridge => {
            host_config.network_mode(dockurl::network::NetworkMode::Bridge);
        }
        dockurl::network::NetworkMode::Host => {
            host_config.extra_host("tfb-server", &config.server_host);
            host_config.extra_host("tfb-database", &config.database_host);
            host_config.network_mode(dockurl::network::NetworkMode::Host);
        }
    }
    host_config.publish_all_ports(true);

    options.host_config(host_config);

    let mut endpoint_settings = EndpointSettings::new();
    endpoint_settings.network_id(config.client_network_id.as_str());

    options.networking_config(NetworkingConfig {
        endpoints_config: EndpointsConfig { endpoint_settings },
    });

    let container_id = dockurl::container::create_container(
        options,
        config.use_unix_socket,
        &config.client_docker_host,
        BuildContainer::new(),
    )?;

    Ok(container_id)
}

/// Creates the container for the `TFBVerifier`.
/// Note: this function makes the assumption that the image has already been
/// pulled from Dockerhub and the Docker daemon is aware of it.
pub fn create_database_verifier_container(
    config: &DockerConfig,
    database_name: &str,
) -> ToolsetResult<String> {
    let mut options = Options::new();
    options.image("techempower/tfb.verifier");
    options.tty(true);
    options.add_env("MODE", "database");
    // These are required but unused.
    options.add_env("PORT", "0");
    options.add_env("ENDPOINT", "");
    options.add_env("TEST_TYPE", "");

    options.add_env("CONCURRENCY_LEVELS", &config.concurrency_levels);
    options.add_env(
        "PIPELINE_CONCURRENCY_LEVELS",
        &config.pipeline_concurrency_levels,
    );
    options.add_env("DATABASE", database_name);

    let mut host_config = HostConfig::new();
    match &config.network_mode {
        dockurl::network::NetworkMode::Bridge => {
            host_config.network_mode(dockurl::network::NetworkMode::Bridge);
        }
        dockurl::network::NetworkMode::Host => {
            host_config.extra_host("tfb-server", &config.server_host);
            host_config.extra_host("tfb-database", &config.database_host);
            host_config.network_mode(dockurl::network::NetworkMode::Host);
        }
    }
    host_config.publish_all_ports(true);

    options.host_config(host_config);

    let mut endpoint_settings = EndpointSettings::new();
    endpoint_settings.network_id(config.client_network_id.as_str());

    options.networking_config(NetworkingConfig {
        endpoints_config: EndpointsConfig { endpoint_settings },
    });

    let container_id = dockurl::container::create_container(
        options,
        config.use_unix_socket,
        &config.client_docker_host,
        BuildContainer::new(),
    )?;

    Ok(container_id)
}

/// Gets both the internal and host port binding for the container given by
/// `container_id`.
pub fn get_port_bindings_for_container(
    docker_config: &DockerConfig,
    docker_host: &str,
    container_id: &str,
) -> ToolsetResult<(String, String)> {
    let inspection = inspect_container(
        container_id,
        docker_host,
        docker_config.use_unix_socket,
        Simple::new(),
    )?;

    if let Some(exposed_ports) = inspection.config.exposed_ports {
        for key in exposed_ports.keys() {
            let inner_port: Vec<&str> = key.split('/').collect();

            match docker_config.network_mode {
                NetworkMode::Bridge => {
                    if let Some(key) = inspection.network_settings.ports.get(key) {
                        if let Some(port_mapping) = key.get(0) {
                            if let Some(inner_port) = inner_port.get(0) {
                                return Ok((
                                    port_mapping.host_port.clone(),
                                    inner_port.to_string(),
                                ));
                            }
                        }
                    }
                }
                NetworkMode::Host => {
                    return Ok((
                        inner_port.get(0).unwrap().to_string(),
                        inner_port.get(0).unwrap().to_string(),
                    ));
                }
            };
        }
    } else {
        return Err(ExposePortError);
    }

    Err(ContainerPortMappingInspectionError)
}

/// Starts the container for the given `Test`.
/// Note: this function makes the assumption that the container is already
/// built and that the docker daemon is aware of it.
/// Call `create_container()` before running.
pub fn start_container(
    docker_config: &DockerConfig,
    container_id: &str,
    docker_host: &str,
    logger: &Logger,
) -> ToolsetResult<()> {
    let cid = container_id.to_string();
    let host = docker_host.to_string();
    let use_unix_socket = docker_config.use_unix_socket;
    let logger = logger.clone();
    thread::spawn(move || {
        attach_to_container(&cid, &host, use_unix_socket, Application::new(&logger)).unwrap();
    });
    dockurl::container::start_container(
        container_id,
        docker_host,
        docker_config.use_unix_socket,
        Simple::new(),
    )?;
    Ok(())
}

/// Retrieves the benchmark commands for the
pub fn start_benchmark_command_retrieval_container(
    docker_config: &DockerConfig,
    test_type: &(&String, &String),
    container_id: &str,
    logger: &Logger,
) -> ToolsetResult<BenchmarkCommands> {
    dockurl::container::start_container(
        container_id,
        &docker_config.client_docker_host,
        docker_config.use_unix_socket,
        Simple::new(),
    )?;
    wait_for_container_to_exit(
        container_id,
        &docker_config.client_docker_host,
        docker_config.use_unix_socket,
        Simple::new(),
    )?;
    let listener = get_container_logs(
        container_id,
        &docker_config.client_docker_host,
        docker_config.use_unix_socket,
        BenchmarkCommandListener::new(test_type, logger),
    )?;

    if docker_config.clean_up {
        delete_container(
            &container_id,
            &docker_config.client_docker_host,
            docker_config.use_unix_socket,
            Simple::new(),
            true,
            true,
            false,
        )?;
    }
    if let Some(commands) = listener.benchmark_commands {
        Ok(commands)
    } else {
        Err(FailedBenchmarkCommandRetrievalError)
    }
}

/// Starts the benchmarker container and logs its stdout/stderr.
pub fn start_benchmarker_container(
    docker_config: &DockerConfig,
    container_id: &str,
    logger: &Logger,
) -> ToolsetResult<BenchmarkResults> {
    dockurl::container::start_container(
        container_id,
        &docker_config.client_docker_host,
        docker_config.use_unix_socket,
        Simple::new(),
    )?;
    wait_for_container_to_exit(
        container_id,
        &docker_config.client_docker_host,
        docker_config.use_unix_socket,
        Simple::new(),
    )?;
    let benchmarker = get_container_logs(
        container_id,
        &docker_config.client_docker_host,
        docker_config.use_unix_socket,
        Benchmarker::new(logger),
    )?;

    if docker_config.clean_up {
        delete_container(
            &container_id,
            &docker_config.client_docker_host,
            docker_config.use_unix_socket,
            Simple::new(),
            true,
            true,
            false,
        )?;
    }

    benchmarker.parse_wrk_output()
}

/// Starts the verification container, captures its stdout/stderr, parses any
/// messages sent from the verifier, and logs the rest.
pub fn start_verification_container(
    docker_config: &DockerConfig,
    project: &Project,
    test: &Test,
    test_type: &(&String, &String),
    container_id: &str,
    logger: &Logger,
) -> ToolsetResult<Verification> {
    let mut to_ret = Verification {
        framework_name: project.framework.get_name(),
        test_name: test.get_name(),
        type_name: test_type.0.clone(),
        warnings: vec![],
        errors: vec![],
    };
    let verification = Arc::new(Mutex::new(to_ret.clone()));

    let verifier_container_id = container_id.to_string();
    let config = docker_config.clone();
    let client_docker_host = config.client_docker_host;
    let use_unix_socket = docker_config.use_unix_socket;
    let verifier_logger = logger.clone();
    let inner_verification = Arc::clone(&verification);
    // This function is extremely complicated and seemingly in the wrong order, but it is very
    // convoluted and intended. We attach to the container *before* it is started in a new thread,
    // and, using an Arc, communicate stderr/stdout and messages from the container (when it runs)
    // to the main thread.
    // `attach_to_container` blocks and therefore must be in a separate thread.
    // If we did `attach` *after* `start_container`, then there is an **INTENDED** implementation
    // in Docker to **NOT** close the connection, so this would block indefinitely.
    // It is safe to trust this implementation in the thread because we `attach` **BEFORE** the
    // container is started, and therefore it *will* exit after we are `attached` which will close
    // the connection.
    thread::spawn(move || {
        dockurl::container::attach_to_container(
            &verifier_container_id,
            &client_docker_host,
            use_unix_socket,
            Verifier::new(Arc::clone(&inner_verification), &verifier_logger),
        )
        .unwrap();
    });

    dockurl::container::start_container(
        &container_id,
        &docker_config.client_docker_host,
        docker_config.use_unix_socket,
        Simple::new(),
    )?;

    wait_for_container_to_exit(
        &container_id,
        &docker_config.client_docker_host,
        docker_config.use_unix_socket,
        Simple::new(),
    )?;

    if docker_config.clean_up {
        delete_container(
            &container_id,
            &docker_config.client_docker_host,
            docker_config.use_unix_socket,
            Simple::new(),
            true,
            true,
            false,
        )?;
    }

    if let Ok(verification) = verification.lock() {
        to_ret = verification.clone();
    }

    Ok(to_ret)
}

/// Starts the verification container and blocks until the database is accepting connections.
pub fn block_until_database_is_ready(
    docker_config: &DockerConfig,
    container_id: &str,
) -> ToolsetResult<()> {
    dockurl::container::start_container(
        container_id,
        &docker_config.client_docker_host,
        docker_config.use_unix_socket,
        Simple::new(),
    )?;

    wait_for_container_to_exit(
        container_id,
        &docker_config.client_docker_host,
        docker_config.use_unix_socket,
        Simple::new(),
    )?;

    if docker_config.clean_up {
        delete_container(
            container_id,
            &docker_config.client_docker_host,
            docker_config.use_unix_socket,
            Simple::new(),
            true,
            true,
            false,
        )?;
    }

    Ok(())
}

/// Polls until `container` is ready with either some `container_id` or `None`,
/// then kills that `container_id`, and sets the internal `container_id` to
/// `None`.
///
/// Note: this function blocks until the given `container` is in a ready state.
pub fn stop_docker_container_future(
    use_unix_socket: bool,
    docker_clean_up: bool,
    container_id: &Arc<Mutex<DockerContainerIdFuture>>,
) {
    let mut requires_wait_to_stop = false;
    if let Ok(container) = container_id.lock() {
        requires_wait_to_stop = container.requires_wait_to_stop;
    }
    if requires_wait_to_stop {
        let mut poll = Poll::Pending;
        while poll == Poll::Pending {
            if let Ok(container) = container_id.lock() {
                poll = container.poll();
                if poll == Poll::Pending {
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }
        if let Ok(mut container) = container_id.lock() {
            if let Some(container_id) = &container.container_id {
                kill_container(
                    container_id,
                    &container.docker_host,
                    use_unix_socket,
                    Simple::new(),
                )
                .unwrap_or(());
                // ↑ specifically succeeds even if there is an error
                // For instance, if an application container stops running because the application
                // crashed, we want to call this and continue.

                if docker_clean_up {
                    delete_container(
                        container_id,
                        &container.docker_host,
                        use_unix_socket,
                        Simple::new(),
                        true,
                        true,
                        false,
                    )
                    .unwrap_or(());
                }

                container.unregister();
            }
            if let Some(image_id) = &container.image_id {
                if docker_clean_up {
                    delete_image(
                        image_id,
                        true,
                        false,
                        &container.docker_host,
                        use_unix_socket,
                        Simple::new(),
                    )
                    .unwrap_or(None);

                    // Todo - this is jank... do this better.
                    delete_unused_images(
                        "{\"dangling\":[\"true\"]}",
                        &container.docker_host,
                        use_unix_socket,
                        Simple::new(),
                    )
                    .unwrap_or(());
                }
            }
            container.image_id = None;
        }
    }
}
