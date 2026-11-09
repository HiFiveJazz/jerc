use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::Duration;

use image::DynamicImage;

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

fn load_remote_image(source: &str) -> Result<DynamicImage, String> {
    if !(source.starts_with("https://") || source.starts_with("http://")) {
        return Err("unsupported image URL scheme".to_owned());
    }
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(12)))
        .build();
    let agent = ureq::Agent::new_with_config(config);
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
    let image =
        image::load_from_memory(&bytes).map_err(|error| format!("image decode failed: {error}"))?;
    // Ignore the classic tracking-pixel shape even if its URL did not make the
    // conservative URL filter in MessageBody::html_images().
    if image.width() <= 2 && image.height() <= 2 {
        return Err("tracking pixel ignored".to_owned());
    }
    Ok(image)
}
