use crate::attachments::{self, Attachment};
use crate::store::Store;
use base64::{engine::general_purpose::STANDARD, Engine};
use std::path::{Path, PathBuf};
use uuid::Uuid;

struct Fixture {
    root: PathBuf,
    db: PathBuf,
    room: String,
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("tongxi-attachment-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("hub.db");
        let mut store = Store::open(&db).unwrap();
        let room = store
            .create("附件测试", "direct", &["codex-win".into()])
            .unwrap()
            .id;
        Self {
            root,
            db,
            room,
            store,
        }
    }

    fn metadata_content(items: &[Attachment]) -> String {
        format!(
            "请检查附件\n\n{}{}\n```",
            attachments::OPEN,
            serde_json::to_string(items).unwrap()
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn read(path: &str) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

#[test]
fn attachment_bytes_metadata_and_same_name_files_are_isolated_and_survive_reopen() {
    let fixture = Fixture::new();
    let first_bytes = b"first payload";
    let second_bytes = b"second payload";
    let first = fixture
        .store
        .stage_attachment(
            &fixture.room,
            "attachment.json",
            "text/plain",
            &STANDARD.encode(first_bytes),
        )
        .unwrap();
    let second = fixture
        .store
        .stage_attachment(
            &fixture.room,
            "attachment.json",
            "text/plain",
            &STANDARD.encode(second_bytes),
        )
        .unwrap();

    assert_eq!(first.bytes, first_bytes.len());
    assert_eq!(read(&first.path), first_bytes);
    assert_eq!(read(&second.path), second_bytes);
    assert_ne!(first.id, second.id);
    assert!(Path::new(&first.path).ends_with("data/attachment.json"));
    let metadata_path = Path::new(&first.path)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("attachment.json");
    let first_meta: Attachment =
        serde_json::from_slice(&std::fs::read(metadata_path).unwrap()).unwrap();
    assert_eq!(first_meta.id, first.id);

    let reopened = Store::open(&fixture.db).unwrap();
    assert_eq!(read(&first.path), first_bytes);
    assert_eq!(
        reopened.attachment_data(&fixture.room, &first.id).unwrap(),
        format!("data:text/plain;base64,{}", STANDARD.encode(first_bytes))
    );
}

#[test]
fn attachment_lookup_rejects_other_room_and_metadata_path_escape() {
    let mut fixture = Fixture::new();
    let item = fixture
        .store
        .stage_attachment(
            &fixture.room,
            "safe.txt",
            "text/plain",
            &STANDARD.encode(b"inside"),
        )
        .unwrap();
    let other_room = fixture
        .store
        .create("另一会话", "direct", &["dsh-win".into()])
        .unwrap()
        .id;
    assert!(fixture
        .store
        .attachment_data(&other_room, &item.id)
        .is_err());
    assert!(attachments::prompt(
        &fixture.db,
        &other_room,
        &Fixture::metadata_content(std::slice::from_ref(&item)),
        false
    )
    .is_err());

    let outside = fixture.root.join("outside.txt");
    std::fs::write(&outside, b"outside").unwrap();
    let metadata_path = Path::new(&item.path)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("attachment.json");
    let mut metadata: Attachment =
        serde_json::from_slice(&std::fs::read(&metadata_path).unwrap()).unwrap();
    metadata.path = outside.to_string_lossy().into_owned();
    std::fs::write(metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    assert!(fixture
        .store
        .attachment_data(&fixture.room, &item.id)
        .is_err());
    assert!(attachments::prompt(
        &fixture.db,
        &fixture.room,
        &Fixture::metadata_content(&[item]),
        false
    )
    .is_err());
}

#[test]
fn image_requires_image_bytes_and_prompt_uses_local_paths_without_file_payloads() {
    let fixture = Fixture::new();
    let fake_image = fixture
        .store
        .stage_attachment(
            &fixture.room,
            "fake.png",
            "image/png",
            &STANDARD.encode(b"plain text pretending to be a PNG"),
        )
        .unwrap();
    assert_eq!(fake_image.mime, "application/octet-stream");

    let image_bytes = b"\x89PNG\r\n\x1a\nfixture image bytes";
    let image = fixture
        .store
        .stage_attachment(
            &fixture.room,
            "real.png",
            "application/octet-stream",
            &STANDARD.encode(image_bytes),
        )
        .unwrap();
    assert_eq!(image.mime, "image/png");

    let content = Fixture::metadata_content(&[fake_image.clone(), image.clone()]);
    assert_eq!(attachments::items(&content).len(), 2);
    assert_eq!(attachments::text(&content), "请检查附件");
    let input = attachments::prompt(&fixture.db, &fixture.room, &content, true).unwrap();
    let local_images: Vec<_> = input
        .iter()
        .filter(|part| part["type"] == "localImage")
        .collect();
    assert_eq!(local_images.len(), 1);
    assert_eq!(local_images[0]["path"], image.path);
    let body = input[0]["text"].as_str().unwrap();
    assert!(body.contains(&fake_image.path));
    assert!(body.contains(&image.path));
    assert!(body.contains("按需用原生工具读取"));
    assert!(!body.contains(&STANDARD.encode(image_bytes)));

    let native_input = attachments::prompt(&fixture.db, &fixture.room, &content, false).unwrap();
    assert_eq!(native_input.len(), 1);
    assert!(native_input[0]["text"]
        .as_str()
        .unwrap()
        .contains(&image.path));
    assert!(native_input.iter().all(|part| part["type"] != "localImage"));
}

#[test]
fn malformed_metadata_and_invalid_or_oversized_base64_are_rejected() {
    let fixture = Fixture::new();
    assert!(fixture
        .store
        .stage_attachment(&fixture.room, "x", "text/plain", "%%%invalid%%%")
        .is_err());
    // 32 MiB + 1 raw bytes, represented by its encoded length; staging must reject it before decode.
    let oversized = "A".repeat(44_739_245);
    assert!(fixture
        .store
        .stage_attachment(
            &fixture.room,
            "large.bin",
            "application/octet-stream",
            &oversized
        )
        .is_err());
    assert!(fixture
        .store
        .stage_attachment(&fixture.room, "... ", "text/plain", &STANDARD.encode(b"x"))
        .is_err());

    let valid = fixture
        .store
        .stage_attachment(
            &fixture.room,
            "ok.txt",
            "text/plain",
            &STANDARD.encode(b"ok"),
        )
        .unwrap();
    let wrong_room = Uuid::new_v4().to_string();
    assert!(fixture
        .store
        .attachment_data(&wrong_room, &valid.id)
        .is_err());
    assert!(attachments::items("body\n```tongxi-attachments\n{broken}\n```").is_empty());
    assert_eq!(
        attachments::text("body\n```tongxi-attachments\n{broken}\n```"),
        "body\n```tongxi-attachments\n{broken}\n```"
    );
}

#[cfg(windows)]
#[test]
fn native_cf_hdrop_is_read_in_a_private_window_station_child() {
    if std::env::var_os("TONGXI_PRIVATE_CLIPBOARD_CHILD").is_some() {
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "attachment_tests::native_cf_hdrop_private_station_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("TONGXI_PRIVATE_CLIPBOARD_CHILD", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("private CF_HDROP child exceeded the 15 second test bound");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "private CF_HDROP child failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.contains("PASS private-station CF_HDROP") {
        println!("PASS private-station CF_HDROP round trip verified");
    } else if let Some(reason) = output
        .lines()
        .find(|line| line.contains("SKIP private-station CF_HDROP"))
    {
        println!("{reason}");
    } else {
        panic!("private clipboard child returned no verification result: {output}");
    }
}

#[cfg(windows)]
#[test]
fn native_cf_hdrop_private_station_child() {
    if std::env::var_os("TONGXI_PRIVATE_CLIPBOARD_CHILD").is_none() {
        return;
    }
    match private_station_hdrop_round_trip() {
        Ok(()) => println!("PASS private-station CF_HDROP reached Store.paste_clipboard_files"),
        Err(reason) => {
            eprintln!("SKIP private-station CF_HDROP: {reason}");
        }
    }
}

#[cfg(windows)]
fn private_station_hdrop_round_trip() -> Result<(), String> {
    use std::ffi::c_void;

    type Handle = *mut c_void;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetProcessWindowStation() -> Handle;
        fn SetProcessWindowStation(station: Handle) -> i32;
        fn CreateWindowStationW(
            name: *const u16,
            flags: u32,
            access: u32,
            security: Handle,
        ) -> Handle;
        fn CloseWindowStation(station: Handle) -> i32;
        fn GetThreadDesktop(thread_id: u32) -> Handle;
        fn SetThreadDesktop(desktop: Handle) -> i32;
        fn CreateDesktopW(
            name: *const u16,
            device: Handle,
            devmode: Handle,
            flags: u32,
            access: u32,
            security: Handle,
        ) -> Handle;
        fn CloseDesktop(desktop: Handle) -> i32;
        fn GetCurrentThreadId() -> u32;
        fn OpenClipboard(window: Handle) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, data: Handle) -> Handle;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> Handle;
        fn GlobalLock(memory: Handle) -> *mut c_void;
        fn GlobalUnlock(memory: Handle) -> i32;
        fn GlobalFree(memory: Handle) -> Handle;
    }

    struct PrivateStation {
        original_station: Handle,
        original_desktop: Handle,
        station: Handle,
        desktop: Handle,
    }
    impl Drop for PrivateStation {
        fn drop(&mut self) {
            unsafe {
                if !self.original_desktop.is_null() {
                    SetThreadDesktop(self.original_desktop);
                }
                if !self.original_station.is_null() {
                    SetProcessWindowStation(self.original_station);
                }
                if !self.desktop.is_null() {
                    CloseDesktop(self.desktop);
                }
                if !self.station.is_null() {
                    CloseWindowStation(self.station);
                }
            }
        }
    }

    let original_station = unsafe { GetProcessWindowStation() };
    let original_desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    if original_station.is_null() || original_desktop.is_null() {
        return Err("could not capture this test process station handles".into());
    }
    let name: Vec<u16> = format!("TongxiAttachmentTest_{}", Uuid::new_v4())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let station =
        unsafe { CreateWindowStationW(name.as_ptr(), 0, 0x000F_037F, std::ptr::null_mut()) };
    if station.is_null() {
        return Err("CreateWindowStationW was denied".into());
    }
    let mut private = PrivateStation {
        original_station,
        original_desktop,
        station,
        desktop: std::ptr::null_mut(),
    };
    if unsafe { SetProcessWindowStation(private.station) } == 0 {
        return Err("SetProcessWindowStation was denied".into());
    }
    let desktop_name: Vec<u16> = "Default".encode_utf16().chain(std::iter::once(0)).collect();
    private.desktop = unsafe {
        CreateDesktopW(
            desktop_name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            0x0000_0101,
            std::ptr::null_mut(),
        )
    };
    if private.desktop.is_null() || unsafe { SetThreadDesktop(private.desktop) } == 0 {
        return Err("creating or selecting the private clipboard desktop was denied".into());
    }

    let fixture = Fixture::new();
    let source_path = fixture.root.join("synthetic-clipboard.txt");
    let expected = b"private CF_HDROP synthetic file bytes";
    std::fs::write(&source_path, expected).map_err(|e| e.to_string())?;
    let wide_path: Vec<u16> = source_path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .chain(std::iter::once(0))
        .collect();
    // DROPFILES is a 20-byte header followed by a UTF-16 multi-string.
    let mut dropfiles = vec![0u8; 20];
    dropfiles[..4].copy_from_slice(&20u32.to_le_bytes());
    dropfiles[16..20].copy_from_slice(&1u32.to_le_bytes()); // fWide = TRUE
    dropfiles.extend(wide_path.iter().flat_map(|word| word.to_le_bytes()));
    let global = unsafe { GlobalAlloc(0x0002, dropfiles.len()) }; // GMEM_MOVEABLE
    if global.is_null() {
        return Err("GlobalAlloc for synthetic CF_HDROP failed".into());
    }
    let pointer = unsafe { GlobalLock(global) };
    if pointer.is_null() {
        unsafe {
            GlobalFree(global);
        }
        return Err("GlobalLock for synthetic CF_HDROP failed".into());
    }
    unsafe {
        std::ptr::copy_nonoverlapping(dropfiles.as_ptr(), pointer.cast::<u8>(), dropfiles.len());
        GlobalUnlock(global);
    }
    if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
        unsafe {
            GlobalFree(global);
        }
        return Err("OpenClipboard failed on the private station".into());
    }
    let emptied = unsafe { EmptyClipboard() } != 0;
    let installed = emptied && !unsafe { SetClipboardData(15, global) }.is_null();
    unsafe {
        CloseClipboard();
        if !installed {
            // Ownership transfers only after SetClipboardData succeeds.
            GlobalFree(global);
        }
    }
    if !installed {
        return Err("installing synthetic CF_HDROP failed on the private station".into());
    }

    let attachments = fixture.store.paste_clipboard_files(&fixture.room)?;
    if attachments.len() != 1 || read(&attachments[0].path) != expected {
        return Err("Store did not stage exactly the synthetic file bytes".into());
    }
    Ok(())
}
