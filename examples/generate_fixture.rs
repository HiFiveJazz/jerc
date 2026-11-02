use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let root = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/aerc-rs-maildir"));
    let count = env::args()
        .nth(2)
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(10_000);

    for subdir in ["cur", "new", "tmp"] {
        fs::create_dir_all(root.join(subdir)).unwrap();
    }
    let info_separator = if cfg!(windows) { '!' } else { ':' };
    for i in 0..count {
        let flags = if i % 3 == 0 { "S" } else { "" };
        let path = root
            .join("cur")
            .join(format!("{i:08}{info_separator}2,{flags}"));
        let mail = format!(
            "From: Sender {i} <sender{i}@example.com>\r\nSubject: [PATCH {}/{}] synthetic benchmark message\r\nDate: Sun, 1 Nov 2026 10:{:02}:00 -0800\r\nMessage-ID: <fixture-{i}@example.com>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nThis body should not be read during header scanning.\r\n",
            i + 1,
            count,
            i % 60,
        );
        fs::write(path, mail).unwrap();
    }
    eprintln!("generated {count} messages in {}", root.display());
}
