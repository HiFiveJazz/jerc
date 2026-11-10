use std::net::IpAddr;
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::Duration;

use image::DynamicImage;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};
use ureq::{Error as HttpError, config::Config, http::Uri};
use url::Url;

use crate::worker::RuntimeEvent;

const IMAGE_QUEUE: usize = 8;
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug)]
pub struct MailImageEvent {
    pub source: String,
    pub result: Result<DynamicImage, String>,
}

pub struct MailImageLoader {
    sender: SyncSender<String>,
}

impl MailImageLoader {
    #[must_use]
    pub fn spawn(runtime_tx: SyncSender<RuntimeEvent>) -> Self {
        let (sender, receiver) = sync_channel::<String>(IMAGE_QUEUE);
        thread::Builder::new()
            .name("mail-image-loader".into())
            .spawn(move || {
                while let Ok(source) = receiver.recv() {
                    let result = load_remote_image(&source);
                    if runtime_tx
                        .send(RuntimeEvent::MailImage(MailImageEvent { source, result }))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("failed to spawn mail image loader");
        Self { sender }
    }

    pub fn request(&self, source: String) -> Result<(), TrySendError<String>> {
        self.sender.try_send(source)
    }
}

/// Defense in depth: never fetch private or loopback IPs, non-TLS URLs,
/// nonstandard ports, userinfo, or redirects. No environment proxies can
/// override the destination restrictions.
fn image_host(source: &str) -> Result<String, String> {
    if source.len() > 8192 {
        return Err("remote image URL is too long".into());
    }
    let url = Url::parse(source).map_err(|_| "invalid remote image URL".to_owned())?;
    if url.scheme() != "https" || url.port_or_known_default() != Some(443) {
        return Err("remote images require HTTPS on port 443".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("remote image URLs cannot contain credentials".into());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "remote image URL has no host".to_owned())?;
    if host.parse::<IpAddr>().is_ok()
        || !host.contains('.')
        || host.ends_with('.')
        || [
            ".localhost",
            ".local",
            ".internal",
            ".test",
            ".invalid",
            ".example",
        ]
        .iter()
        .any(|suffix| host.to_ascii_lowercase().ends_with(suffix))
    {
        return Err("remote image host is not a public DNS hostname".into());
    }
    Ok(host.to_owned())
}

fn public_image_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            // Reject RFC1918, CGNAT, loopback, link-local, documentation,
            // benchmarking, unspecified, multicast and reserved ranges.
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192
                    && (b == 168 || (b == 0 && (c == 0 || c == 2)) || (b == 88 && c == 99)))
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            // Only globally scoped unicast; ::ffff IPv4-mapped, fc00::/7,
            // fe80::/10, documentation, and special-purpose ranges are denied.
            segments[0] & 0xe000 == 0x2000 && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
        }
    }
}

/// Validate the same DNS result that the TCP connector will use. This avoids
/// a separate preflight lookup, which would be vulnerable to DNS rebinding.
#[derive(Debug)]
struct PublicImageResolver;

impl Resolver for PublicImageResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, HttpError> {
        let addresses = DefaultResolver::default().resolve(uri, config, timeout)?;
        if addresses.is_empty() || addresses.iter().any(|addr| !public_image_ip(addr.ip())) {
            return Err(HttpError::ConnectionFailed);
        }
        Ok(addresses)
    }
}

fn load_remote_image(source: &str) -> Result<DynamicImage, String> {
    image_host(source)?;
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(12)))
        .https_only(true)
        .max_redirects(0)
        .proxy(None)
        .build();
    // Keep TLS certificate validation and SNI for the original hostname,
    // while routing every socket through the validating DNS resolver.
    let agent = ureq::Agent::with_parts(config, DefaultConnector::default(), PublicImageResolver);
    let mut response = agent
        .get(source)
        .call()
        .map_err(|error| format!("image download failed: {error}"))?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MAX_IMAGE_BYTES)
        .read_to_vec()
        .map_err(|error| format!("image download failed: {error}"))?;
    // The compressed download cap is insufficient for a decompression bomb.
    // Reject oversized dimensions before allocating the decoded raster.
    let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|error| format!("image format failed: {error}"))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|error| format!("image decode failed: {error}"))?;
    // Ignore the classic tracking-pixel shape even if its URL did not make the
    // conservative URL filter in MessageBody::html_images().
    if image.width() <= 2 && image.height() <= 2 {
        return Err("tracking pixel ignored".to_owned());
    }
    Ok(image)
}

#[cfg(test)]
mod security_tests {
    use super::{image_host, public_image_ip};
    use std::net::IpAddr;

    #[test]
    fn remote_images_reject_unsafe_schemes_hosts_and_ports() {
        for url in [
            "http://example.com/pic.png",
            "https://127.0.0.1/pic.png",
            "https://[::1]/pic.png",
            "https://localhost/pic.png",
            "https://printer.local/pic.png",
            "https://internal.test/pic.png",
            "https://user:pass@example.com/pic.png",
            "https://example.com:8443/pic.png",
        ] {
            assert!(image_host(url).is_err(), "should reject {url}");
        }
        assert_eq!(
            image_host("https://cdn.example.org/pic.png").unwrap(),
            "cdn.example.org"
        );
    }

    #[test]
    fn remote_images_reject_private_dns_answers() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "100.64.0.1",
            "169.254.169.254",
            "172.20.1.1",
            "198.18.0.1",
            "192.0.2.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "2001:db8::1",
        ] {
            assert!(!public_image_ip(ip.parse::<IpAddr>().unwrap()), "{ip}");
        }
        assert!(public_image_ip("8.8.8.8".parse().unwrap()));
        assert!(public_image_ip("2606:4700:4700::1111".parse().unwrap()));
    }
}
