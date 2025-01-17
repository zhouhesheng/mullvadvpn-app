use os_pipe::{pipe, PipeWriter};
use parking_lot::Mutex;
use shell_escape;
use std::{
    ffi::{OsStr, OsString},
    fmt, io,
    path::{Path, PathBuf},
};
use talpid_types::{net, ErrorExt};

static BASE_ARGUMENTS: &[&[&str]] = &[
    &["--client"],
    &["--tls-client"],
    &["--nobind"],
    &["--mute-replay-warnings"],
    #[cfg(not(windows))]
    &["--dev", "tun"],
    #[cfg(windows)]
    &["--dev-type", "tun"],
    &["--ping", "4"],
    &["--ping-exit", "25"],
    &["--connect-timeout", "30"],
    &["--connect-retry", "0", "0"],
    &["--connect-retry-max", "1"],
    &["--remote-cert-tls", "server"],
    &["--rcvbuf", "1048576"],
    &["--sndbuf", "1048576"],
    &["--fast-io"],
    &["--data-ciphers-fallback", "AES-256-GCM"],
    &["--tls-version-min", "1.3"],
    &["--verb", "3"],
    #[cfg(windows)]
    &[
        "--route-gateway",
        "dhcp",
        "--route",
        "0.0.0.0",
        "0.0.0.0",
        "vpn_gateway",
        "1",
    ],
    // The route manager is used to add the routes.
    #[cfg(target_os = "linux")]
    &["--route-noexec"],
    #[cfg(windows)]
    &["--ip-win32", "ipapi"],
    #[cfg(windows)]
    &["--windows-driver", "wintun"],
];

static ALLOWED_TLS1_3_CIPHERS: &[&str] =
    &["TLS_AES_256_GCM_SHA384", "TLS_CHACHA20_POLY1305_SHA256"];

/// An OpenVPN process builder, providing control over the different arguments that the OpenVPN
/// binary accepts.
#[derive(Clone)]
pub struct OpenVpnCommand {
    openvpn_bin: OsString,
    config: Option<PathBuf>,
    remote: Option<net::Endpoint>,
    user_pass_path: Option<PathBuf>,
    proxy_auth_path: Option<PathBuf>,
    ca: Option<PathBuf>,
    crl: Option<PathBuf>,
    plugin: Option<(PathBuf, Vec<String>)>,
    log: Option<PathBuf>,
    tunnel_options: net::openvpn::TunnelOptions,
    proxy_settings: Option<net::openvpn::ProxySettings>,
    tunnel_alias: Option<OsString>,
    enable_ipv6: bool,
    proxy_port: Option<u16>,
    #[cfg(target_os = "linux")]
    fwmark: Option<u32>,
}

impl OpenVpnCommand {
    /// Constructs a new `OpenVpnCommand` for launching OpenVPN processes from the binary at
    /// `openvpn_bin`.
    pub fn new<P: AsRef<OsStr>>(openvpn_bin: P) -> Self {
        OpenVpnCommand {
            openvpn_bin: OsString::from(openvpn_bin.as_ref()),
            config: None,
            remote: None,
            user_pass_path: None,
            proxy_auth_path: None,
            ca: None,
            crl: None,
            plugin: None,
            log: None,
            tunnel_options: net::openvpn::TunnelOptions::default(),
            proxy_settings: None,
            tunnel_alias: None,
            enable_ipv6: true,
            proxy_port: None,
            #[cfg(target_os = "linux")]
            fwmark: None,
        }
    }

    /// Sets what the firewall mark should be
    #[cfg(target_os = "linux")]
    pub fn fwmark(&mut self, fwmark: Option<u32>) -> &mut Self {
        self.fwmark = fwmark;
        self
    }

    /// Sets what configuration file will be given to OpenVPN
    pub fn config(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.config = Some(path.as_ref().to_path_buf());
        self
    }

    /// Sets the address and protocol that OpenVPN will connect to.
    pub fn remote(&mut self, remote: net::Endpoint) -> &mut Self {
        self.remote = Some(remote);
        self
    }

    /// Sets the path to the file where the username and password for user-pass authentication
    /// is stored. See the `--auth-user-pass` OpenVPN documentation for details.
    pub fn user_pass(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.user_pass_path = Some(path.as_ref().to_path_buf());
        self
    }

    /// Sets the path to the file where the username and password for proxy authentication
    /// is stored.
    pub fn proxy_auth(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.proxy_auth_path = Some(path.as_ref().to_path_buf());
        self
    }

    /// Sets the path to the CA certificate file.
    pub fn ca(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.ca = Some(path.as_ref().to_path_buf());
        self
    }

    /// Sets the path to the CRL (Certificate revocation list) file.
    pub fn crl(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.crl = Some(path.as_ref().to_path_buf());
        self
    }

    /// Sets a plugin and its arguments that OpenVPN will be started with.
    pub fn plugin(&mut self, path: impl AsRef<Path>, args: Vec<String>) -> &mut Self {
        self.plugin = Some((path.as_ref().to_path_buf(), args));
        self
    }

    /// Sets a log file path.
    pub fn log(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.log = Some(path.as_ref().to_path_buf());
        self
    }

    /// Sets extra options
    pub fn tunnel_options(&mut self, tunnel_options: &net::openvpn::TunnelOptions) -> &mut Self {
        self.tunnel_options = tunnel_options.clone();
        self
    }

    /// Sets the tunnel alias which will be used to identify a tunnel device that will be used by
    /// OpenVPN.
    pub fn tunnel_alias(&mut self, tunnel_alias: Option<OsString>) -> &mut Self {
        self.tunnel_alias = tunnel_alias;
        self
    }

    /// Configures if IPv6 should be allowed in the tunnel.
    pub fn enable_ipv6(&mut self, enable_ipv6: bool) -> &mut Self {
        self.enable_ipv6 = enable_ipv6;
        self
    }

    /// Sets the local proxy port bound to.
    /// In case of dynamic port selection, this will only be known after the proxy has been started.
    pub fn proxy_port(&mut self, proxy_port: u16) -> &mut Self {
        self.proxy_port = Some(proxy_port);
        self
    }

    /// Sets the proxy settings.
    pub fn proxy_settings(&mut self, proxy_settings: net::openvpn::ProxySettings) -> &mut Self {
        self.proxy_settings = Some(proxy_settings);
        self
    }

    /// Build a runnable expression from the current state of the command.
    pub fn build(&self) -> tokio::process::Command {
        log::debug!("Building expression: {}", &self);
        let mut handle = tokio::process::Command::new(&self.openvpn_bin);
        handle.args(self.get_arguments());
        handle
    }

    /// Returns all arguments that the subprocess would be spawned with.
    fn get_arguments(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = Self::base_arguments().iter().map(OsString::from).collect();

        if let Some(ref config) = self.config {
            args.push(OsString::from("--config"));
            args.push(OsString::from(config.as_os_str()));
        }

        args.extend(self.remote_arguments().iter().map(OsString::from));
        args.extend(self.authentication_arguments());

        if let Some(ref ca) = self.ca {
            args.push(OsString::from("--ca"));
            args.push(OsString::from(ca.as_os_str()));
        }
        if let Some(ref crl) = self.crl {
            args.push(OsString::from("--crl-verify"));
            args.push(OsString::from(crl.as_os_str()));
        }

        if let Some((ref path, ref plugin_args)) = self.plugin {
            args.push(OsString::from("--plugin"));
            args.push(OsString::from(path));
            args.extend(plugin_args.iter().map(OsString::from));
        }

        if let Some(ref path) = self.log {
            args.push(OsString::from("--log"));
            args.push(OsString::from(path))
        }

        if let Some(mssfix) = self.tunnel_options.mssfix {
            args.push(OsString::from("--mssfix"));
            args.push(OsString::from(mssfix.to_string()));
        }

        if !self.enable_ipv6 {
            args.push(OsString::from("--pull-filter"));
            args.push(OsString::from("ignore"));
            args.push(OsString::from("route-ipv6"));

            args.push(OsString::from("--pull-filter"));
            args.push(OsString::from("ignore"));
            args.push(OsString::from("ifconfig-ipv6"));
        }

        if let Some(ref tunnel_device) = self.tunnel_alias {
            args.push(OsString::from("--dev-node"));
            args.push(tunnel_device.clone());
        }

        args.extend(Self::tls_cipher_arguments().iter().map(OsString::from));
        args.extend(self.proxy_arguments().iter().map(OsString::from));

        #[cfg(target_os = "linux")]
        if let Some(mark) = &self.fwmark {
            args.extend(["--mark", &mark.to_string()].iter().map(OsString::from));
        }

        args
    }

    fn base_arguments() -> Vec<&'static str> {
        let mut args = vec![];
        for arglist in BASE_ARGUMENTS.iter() {
            for arg in arglist.iter() {
                args.push(*arg);
            }
        }
        args
    }

    fn tls_cipher_arguments() -> Vec<String> {
        vec![
            "--tls-ciphersuites".to_owned(),
            ALLOWED_TLS1_3_CIPHERS.join(":"),
        ]
    }

    fn remote_arguments(&self) -> Vec<String> {
        let mut args: Vec<String> = vec![];
        if let Some(ref endpoint) = self.remote {
            args.push("--proto".to_owned());
            args.push(match endpoint.protocol {
                net::TransportProtocol::Udp => "udp".to_owned(),
                net::TransportProtocol::Tcp => "tcp-client".to_owned(),
            });
            args.push("--remote".to_owned());
            args.push(endpoint.address.ip().to_string());
            args.push(endpoint.address.port().to_string());
        }
        args
    }

    fn authentication_arguments(&self) -> Vec<OsString> {
        let mut args = vec![];
        if let Some(ref user_pass_path) = self.user_pass_path {
            args.push(OsString::from("--auth-user-pass"));
            args.push(OsString::from(user_pass_path));
        }
        args
    }

    fn proxy_arguments(&self) -> Vec<String> {
        let mut args = vec![];
        match self.proxy_settings {
            Some(net::openvpn::ProxySettings::Local(ref local_proxy)) => {
                args.push("--socks-proxy".to_owned());
                args.push("127.0.0.1".to_owned());
                args.push(local_proxy.port.to_string());
                args.push("--route".to_owned());
                args.push(local_proxy.peer.ip().to_string());
                args.push("255.255.255.255".to_owned());
                args.push("net_gateway".to_owned());
            }
            Some(net::openvpn::ProxySettings::Remote(ref remote_proxy)) => {
                args.push("--socks-proxy".to_owned());
                args.push(remote_proxy.address.ip().to_string());
                args.push(remote_proxy.address.port().to_string());

                if let Some(ref _auth) = remote_proxy.auth {
                    if let Some(ref auth_file) = self.proxy_auth_path {
                        args.push(auth_file.to_string_lossy().to_string());
                    } else {
                        log::error!("Proxy credentials present but credentials file missing");
                    }
                }

                args.push("--route".to_owned());
                args.push(remote_proxy.address.ip().to_string());
                args.push("255.255.255.255".to_owned());
                args.push("net_gateway".to_owned());
            }
            Some(net::openvpn::ProxySettings::Shadowsocks(ref ss)) => {
                args.push("--socks-proxy".to_owned());
                args.push("127.0.0.1".to_owned());

                if let Some(ref proxy_port) = self.proxy_port {
                    args.push(proxy_port.to_string());
                } else {
                    panic!("Dynamic proxy port was not registered with OpenVpnCommand");
                }

                args.push("--route".to_owned());
                args.push(ss.peer.ip().to_string());
                args.push("255.255.255.255".to_owned());
                args.push("net_gateway".to_owned());
            }
            None => {}
        };
        args
    }
}

impl fmt::Display for OpenVpnCommand {
    /// Format the program and arguments of an `OpenVpnCommand` for display. Any non-utf8 data
    /// is lossily converted using the utf8 replacement character.
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.write_str(&shell_escape::escape(self.openvpn_bin.to_string_lossy()))?;
        for arg in &self.get_arguments() {
            fmt.write_str(" ")?;
            fmt.write_str(&shell_escape::escape(arg.to_string_lossy()))?;
        }
        Ok(())
    }
}

/// Handle to a running OpenVPN process.
pub struct OpenVpnProcHandle {
    /// Handle to the child process running OpenVPN.
    ///
    /// This handle is acquired by calling [`OpenVpnCommand::build`] (or
    /// [`tokio::process::Command::spawn`]).
    pub inner: std::sync::Arc<tokio::sync::Mutex<tokio::process::Child>>,
    /// Pipe handle to stdin of the OpenVPN process. Our custom fork of OpenVPN
    /// has been changed so that it exits cleanly when stdin is closed. This is a hack
    /// solution to cleanly shut OpenVPN down without using the
    /// management interface (which would be the correct thing to do).
    pub stdin: Mutex<Option<PipeWriter>>,
}

impl OpenVpnProcHandle {
    /// Configures the expression to run OpenVPN in a way compatible with this handle
    /// and spawns it. Returns the handle.
    pub fn new(mut cmd: &mut tokio::process::Command) -> io::Result<Self> {
        use std::io::IsTerminal;

        if !std::io::stdout().is_terminal() {
            cmd = cmd.stdout(std::process::Stdio::null())
        }

        if !std::io::stderr().is_terminal() {
            cmd = cmd.stderr(std::process::Stdio::null())
        }

        let (reader, writer) = pipe()?;
        let proc_handle = cmd.stdin(reader).spawn()?;

        Ok(Self {
            inner: std::sync::Arc::new(tokio::sync::Mutex::new(proc_handle)),
            stdin: Mutex::new(Some(writer)),
        })
    }

    /// Attempts to stop the OpenVPN process gracefully in the given time
    /// period, otherwise kills the process.
    pub async fn nice_kill(&self, timeout: std::time::Duration) -> io::Result<()> {
        log::debug!("Trying to stop child process gracefully");
        self.stop().await;

        // Wait for the process to die for a maximum of `timeout`.
        let wait_result = tokio::time::timeout(timeout, self.wait()).await;
        match wait_result {
            Ok(_) => log::debug!("Child process terminated gracefully"),
            Err(_) => {
                log::warn!(
                "Child process did not terminate gracefully within timeout, forcing termination"
            );
                self.kill().await?;
            }
        }
        Ok(())
    }

    /// Waits for the child to exit completely, returning the status that it
    /// exited with. See [tokio::process::Child::wait] for in-depth
    /// documentation.
    async fn wait(&self) -> io::Result<std::process::ExitStatus> {
        self.inner.lock().await.wait().await
    }

    /// Kill the OpenVPN process and drop its stdin handle.
    async fn stop(&self) {
        // Dropping our stdin handle so that it is closed once. Closing the handle should
        // gracefully stop our OpenVPN child process.
        if self.stdin.lock().take().is_none() {
            log::warn!("Tried to close OpenVPN stdin handle twice, this is a bug");
        }
        self.clean_up().await
    }

    async fn kill(&self) -> io::Result<()> {
        log::warn!("Killing OpenVPN process");
        self.inner.lock().await.kill().await?;
        log::debug!("OpenVPN forcefully killed");
        Ok(())
    }

    async fn has_stopped(&self) -> io::Result<bool> {
        let exit_status = self.inner.lock().await.try_wait()?;
        Ok(exit_status.is_some())
    }

    /// Try to kill the OpenVPN process.
    async fn clean_up(&self) {
        let result = match self.has_stopped().await {
            Ok(false) => self.kill().await,
            Err(e) => {
                log::error!(
                    "{}",
                    e.display_chain_with_msg("Failed to check if OpenVPN is running")
                );
                self.kill().await
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            log::error!("{}", error.display_chain_with_msg("Failed to kill OpenVPN"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OpenVpnCommand;
    use std::{ffi::OsString, net::Ipv4Addr};
    use talpid_types::net::{Endpoint, TransportProtocol};

    #[test]
    fn passes_one_remote() {
        let remote = Endpoint::new(Ipv4Addr::new(127, 0, 0, 1), 3333, TransportProtocol::Udp);

        let testee_args = OpenVpnCommand::new("").remote(remote).get_arguments();

        assert!(testee_args.contains(&OsString::from("udp")));
        assert!(testee_args.contains(&OsString::from("127.0.0.1")));
        assert!(testee_args.contains(&OsString::from("3333")));
    }

    #[test]
    fn passes_plugin_path() {
        let path = "./a/path";
        let testee_args = OpenVpnCommand::new("").plugin(path, vec![]).get_arguments();
        assert!(testee_args.contains(&OsString::from("./a/path")));
    }

    #[test]
    fn passes_plugin_args() {
        let args = vec![String::from("123"), String::from("cde")];
        let testee_args = OpenVpnCommand::new("").plugin("", args).get_arguments();
        assert!(testee_args.contains(&OsString::from("123")));
        assert!(testee_args.contains(&OsString::from("cde")));
    }
}
