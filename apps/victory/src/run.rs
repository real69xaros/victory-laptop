use std::path::Path;

use anyhow::{Context, Result};

use crate::{Console, Unimplemented};

#[cfg(windows)]
pub fn run(
    exe: &Path,
    unimplemented: Unimplemented,
    strict: bool,
    execute: bool,
    console: Console,
) -> Result<()> {
    use std::sync::Arc;
    use victory_hle::{HleResolver, HostPolicy};
    use victory_loader::{BindReport, UnimplementedPolicy, WithTitleDlls};

    // A title that relaunched itself: the old process must be gone first.
    victory_hle::modules::lifecycle::wait_for_previous();
    let cfg = victory_core::Config::load()?;
    victory_hle::modules::controls::configure(cfg.controls.as_ref())
        .map_err(anyhow::Error::msg)?;
    let original = std::fs::read(exe).with_context(|| format!("reading {}", exe.display()))?;
    let prepared = victory_loader::prepare(&original)?;
    let hash = prepared.original_hash.to_hex();
    match victory_game::identify(&hash) {
        Some(build) => println!("build          {} ({hash})", build.label),
        None => println!("build          unknown ({hash})"),
    }

    let images = cfg.cache_subdir("images")?;
    victory_hle::modules::d3d12x::use_shader_cache(cfg.cache_subdir("shaders")?);
    let image = victory_loader::load(&prepared, &images)?;
    // The program is mapped from the copy `load` wrote: the file's bytes
    // and their patched copy are not read again, and were held for the
    // whole run (two blocks of 114.7 MB each for the title, in
    // physical memory: run 65's memory map).
    drop(prepared);
    drop(original);
    victory_hle::runtime::set_main_image(image.base());
    victory_hle::runtime::set_image_path(image.base(), exe);
    // From the VIctory-GTA6-build-v6 patch (on jmehru/victory-amd-rx9070 aae8d6c).
    hold(image.base(), image.info().size_of_image as usize);
    patch(image.base(), image.info().size_of_image as usize);
    peek(image.base(), image.info().size_of_image as usize);
    println!(
        "mapped at      {:#x} (preferred {:#x})",
        image.base(),
        image.info().image_base
    );
    let resolver = HleResolver::new(if strict {
        HostPolicy::none()
    } else {
        HostPolicy::default()
    });
    let wanted = |dll: &str| title_dll(&resolver.host, dll);
    let title_dir = exe.parent().unwrap_or(Path::new("."));
    // A missing manifest leaves id 0 and no storage quota.
    let manifest =
        std::fs::read_to_string(title_dir.join("MicrosoftGame.config")).unwrap_or_default();
    let title = victory_hle::runtime::Title::from_manifest(&manifest);
    let storage = cfg.data_subdir(&format!("{:08X}/storage", title.id))?;
    println!(
        "title          {:08X}, storage {} ({} MB)",
        title.id,
        storage.display(),
        title.storage_bytes >> 20
    );
    // The console's fixed drives: the package as `G:`, temporary files `T:`.
    victory_hle::modules::files::mount_at('G', title_dir);
    let temp = cfg.data_subdir(&format!("{:08X}/temp", title.id))?;
    victory_hle::modules::files::mount_at('T', &temp);
    victory_hle::runtime::set_title(victory_hle::runtime::Title { storage, ..title });
    victory_hle::console::set(match console {
        Console::SeriesX => victory_hle::console::Console::SeriesX,
        Console::SeriesS => victory_hle::console::Console::SeriesS,
    });
    println!("console        {}", victory_hle::console::current().name());
    let dlls = victory_loader::load_title_dlls(image.info(), title_dir, &images, &wanted)?;
    for (name, dll) in &dlls {
        if name.eq_ignore_ascii_case(victory_hle::modules::ffx::LOADER)
            && let Some(real) = dll.export("ffxDispatch")
        {
            victory_hle::modules::ffx::set_real_dispatch(real);
        }
        println!("title dll      {name} at {:#x}", dll.base());
        victory_hle::runtime::set_image_path(dll.base(), &title_dir.join(name));
    }

    let policy = match unimplemented {
        Unimplemented::Trap => UnimplementedPolicy::Trap,
        Unimplemented::Zero => UnimplementedPolicy::ReturnZero,
    };
    let with_dlls = WithTitleDlls {
        inner: &resolver,
        dlls: &dlls,
    };
    let show = |name: &str, report: BindReport| {
        println!(
            "bound          {name}: {} hle, {} host, {} title, {} missing",
            report.hle,
            report.host,
            report.title,
            report.missing.len()
        );
        for m in &report.missing {
            println!("  missing      {}!{}", m.dll, m.symbol);
        }
    };
    for (name, dll) in &dlls {
        show(name, dll.bind_imports(&with_dlls, policy)?);
    }
    let exe_name = exe.file_name().unwrap_or_default().to_string_lossy();
    // The upscale dispatch is ours (`victory_hle::modules::ffx`); the rest of
    // that DLL is the title's own.
    struct TakeDispatch<'a>(&'a dyn victory_loader::ImportResolver);
    impl victory_loader::ImportResolver for TakeDispatch<'_> {
        fn resolve(
            &self,
            dll: &str,
            symbol: &victory_pe::ImportSymbol,
        ) -> victory_loader::Resolution {
            let ours = dll.eq_ignore_ascii_case(victory_hle::modules::ffx::LOADER)
                && matches!(symbol, victory_pe::ImportSymbol::Name { name, .. } if name == "ffxDispatch");
            if ours {
                return victory_loader::Resolution::Found {
                    address: victory_hle::modules::ffx::dispatch_address(),
                    provenance: victory_loader::Provenance::Hle,
                };
            }
            self.0.resolve(dll, symbol)
        }
    }
    show(&exe_name, image.bind_imports(&TakeDispatch(&with_dlls), policy)?);
    if !execute {
        println!("not executing (pass --execute to start the title)");
        return Ok(());
    }
    victory_loader::report_crashes();
    victory_loader::log_crashes_to(temp.with_file_name("crashes.log"));
    victory_hle::runtime::use_console_timers();
    #[cfg(feature = "diagnostics")]
    diagnostics(&image, &cfg);
    let late = LateDlls {
        host: resolver.host.clone(),
        title_dir: title_dir.to_owned(),
        cache_dir: images,
        policy,
        mapped: std::sync::Mutex::new(dlls.clone()),
        lock: LoaderLock::default(),
    };
    victory_hle::runtime::set_title_dll_loader(Box::new(move |file| {
        late.load(file, show).map_err(|e| format!("{e:#}"))
    }));
    // SAFETY: imports are bound; from here on we run title code.
    let main = unsafe { Arc::new(image).spawn_main(dlls)? };
    let code = main
        .join()
        .map_err(|_| anyhow::anyhow!("title main thread panicked"))??;
    println!("title exited with {code}");
    Ok(())
}

/// The debugging aids (README): scheduled stack dumps, watched values and
/// memory saved on a crash.
#[cfg(all(windows, feature = "diagnostics"))]
fn diagnostics(image: &victory_loader::LoadedImage, cfg: &victory_core::Config) {
    victory_loader::dump_stacks_on_schedule();
    victory_hle::modules::audio::capture::start();
    watch(image.base(), image.info().size_of_image as usize);
    // Debugging aid: `VICTORY_CRASH_SAVE=<rva>:<len>,...` (hex) writes those
    // title ranges to `crash-<rva>.bin` in the cache dir on a crash.
    let size = image.info().size_of_image as usize;
    let ranges = std::env::var("VICTORY_CRASH_SAVE").unwrap_or_default();
    let hex = |s: &str| usize::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    victory_loader::save_on_crash(
        ranges
            .split(',')
            .filter_map(|r| r.split_once(':'))
            .filter_map(|(rva, len)| Some((hex(rva)?, hex(len)?)))
            .filter(|&(rva, len)| rva.checked_add(len).is_some_and(|end| end <= size))
            .map(|(rva, len)| {
                let file = cfg.cache_dir.join(format!("crash-{rva:x}.bin"));
                (image.base() + rva, len, file)
            })
            .collect(),
    );
}

/// Debugging aid: `VICTORY_WATCH=<rva>,...` (hex) logs the u32 at each
/// title+rva whenever it changes, polled every 5 ms (an engine's state, say).
#[cfg(all(windows, feature = "diagnostics"))]
fn watch(base: usize, size: usize) {
    let Ok(list) = std::env::var("VICTORY_WATCH") else {
        return;
    };
    let rvas: Vec<usize> = list
        .split(',')
        .filter_map(|r| usize::from_str_radix(r.trim().trim_start_matches("0x"), 16).ok())
        .filter(|&rva| rva + 4 <= size)
        .collect();
    std::thread::spawn(move || {
        let mut seen = vec![None; rvas.len()];
        loop {
            for (&rva, seen) in rvas.iter().zip(&mut seen) {
                // SAFETY: four bytes inside the mapped image, checked above.
                let value = unsafe { ((base + rva) as *const u32).read_volatile() };
                if *seen != Some(value) {
                    eprintln!("VIctory: title+{rva:#x} = {value:#x}");
                    *seen = Some(value);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
}

/// `VICTORY_PATCH=<rva>:<expected hex>:<new hex>,...`: bytes of the title's
/// code replaced in memory before it runs, each only if the code there is
/// the expected bytes (another build of the title is left alone). The game
/// files are not touched. Grand Theft Auto VI's pool of 224-byte objects
/// (title+0xA9EC938) holds the 22,500 entries its configuration gives; at
/// our frame rates it fills during cutscenes and the title stops at
/// title+0x5149aec (`int 3` on an empty free list).
/// `5AD8E60:E8DB605FFF:B800000100` replaces the configuration call that
/// sizes it with `mov eax, 0x10000` (65,536 entries; its indices hold up to
/// 131,070). With it, the pool of 192-byte objects (title+0x8FE1078) ran
/// empty next (title+0x1fa88e4): `5ABB240:E8FB3C61FF:B800000100` likewise.
/// (Doubling all 596 of the title's pools at the function that makes them
/// stopped it at start, title+0x22106e0: some must keep their size.)
#[cfg(windows)]
fn patch(base: usize, size: usize) {
    let Ok(list) = std::env::var("VICTORY_PATCH") else {
        return;
    };
    let bytes = |h: &str| -> Option<Vec<u8>> {
        let h = h.trim();
        (h.len() % 2 == 0)
            .then(|| (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).ok()).collect())
            .flatten()
    };
    for item in list.split(',') {
        let mut parts = item.split(':');
        let (Some(rva), Some(old), Some(new)) = (parts.next(), parts.next(), parts.next()) else {
            eprintln!("VIctory: patch {item:?} not understood");
            continue;
        };
        let (Ok(rva), Some(old), Some(new)) =
            (usize::from_str_radix(rva.trim().trim_start_matches("0x"), 16), bytes(old), bytes(new))
        else {
            eprintln!("VIctory: patch {item:?} not understood");
            continue;
        };
        if rva + new.len() > size {
            eprintln!("VIctory: patch at title+{rva:#x} outside the image");
            continue;
        }
        let done = victory_loader::patch_code(base + rva, &old, &new);
        eprintln!(
            "VIctory: patch at title+{rva:#x}: {}",
            if done { "applied" } else { "not applied (the code there differs)" }
        );
    }
}

/// Debugging aid: `VICTORY_PEEK=<rva>,...` (hex): every 10 s, the eight
/// dwords at the address the title pointer at each title+rva holds (for a
/// structure reached through a global, such as one of the title's pools).
#[cfg(windows)]
fn peek(base: usize, size: usize) {
    let Ok(list) = std::env::var("VICTORY_PEEK") else {
        return;
    };
    let rvas: Vec<usize> = list
        .split(',')
        .filter_map(|r| usize::from_str_radix(r.trim().trim_start_matches("0x"), 16).ok())
        .filter(|&rva| rva + 8 <= size)
        .collect();
    let started = std::time::Instant::now();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(10));
            for &rva in &rvas {
                // SAFETY: eight bytes inside the mapped image, checked above.
                let ptr = unsafe { ((base + rva) as *const u64).read_volatile() } as usize;
                let mut words = [0u32; 8];
                if ptr != 0 {
                    // SAFETY: a structure the title made at start and keeps
                    // (a pool header), 32 bytes of it.
                    words = unsafe { (ptr as *const [u32; 8]).read_volatile() };
                }
                eprintln!(
                    "VIctory: peek {:>5}s title+{rva:#x} -> {ptr:#x}: {:x?}",
                    started.elapsed().as_secs(),
                    words
                );
            }
        }
    });
}

/// `VICTORY_HOLD=<rva>:<byte>,...` (hex) keeps the byte at each title+rva at
/// that value, rewritten every 100 ms: a title switch held on or off. Grand
/// Theft Auto VI's hang watchdog (`71B6330:1` pauses it) otherwise ends the
/// game when a frame takes seconds (a null write at title+0x23cf683).
#[cfg(windows)]
fn hold(base: usize, size: usize) {
    let Ok(list) = std::env::var("VICTORY_HOLD") else {
        return;
    };
    let hex = |s: &str| usize::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    let held: Vec<(usize, u8)> = list
        .split(',')
        .filter_map(|r| r.split_once(':'))
        .filter_map(|(rva, v)| Some((hex(rva)?, u8::try_from(hex(v)?).ok()?)))
        .filter(|&(rva, _)| rva < size)
        .collect();
    for &(rva, value) in &held {
        eprintln!("VIctory: holding title+{rva:#x} at {value:#x}");
    }
    std::thread::spawn(move || {
        loop {
            for &(rva, value) in &held {
                // SAFETY: one byte inside the mapped image, checked above;
                // the title reads it as a plain flag.
                unsafe { ((base + rva) as *mut u8).write_volatile(value) };
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
}

/// DLLs the host or our runtime provide are never taken from the title.
#[cfg(windows)]
fn title_dll(host: &victory_hle::HostPolicy, dll: &str) -> bool {
    !host.allows(dll) && !victory_hle::registry().iter().any(|m| m.serves(dll))
}

/// Title DLLs the title loads itself (`LoadLibrary`) that nothing imports,
/// e.g. The title's upscaler behind its FidelityFX loader.
#[cfg(windows)]
struct LateDlls {
    host: victory_hle::HostPolicy,
    title_dir: std::path::PathBuf,
    cache_dir: std::path::PathBuf,
    policy: victory_loader::UnimplementedPolicy,
    /// Every title DLL mapped so far, to bind new ones against.
    mapped: std::sync::Mutex<Vec<(String, victory_loader::LoadedImage)>>,
    lock: LoaderLock,
}

#[cfg(windows)]
impl LateDlls {
    /// Every `LoadLibrary` of the title comes here first. Returns a title
    /// image called `file`, mapping it and the title DLLs it needs first,
    /// binding them and running their `DllMain` on the calling thread, as
    /// the Windows loader would; `None` if `file` is no title image.
    fn load(
        &self,
        file: &str,
        show: impl Fn(&str, victory_loader::BindReport),
    ) -> Result<Option<usize>> {
        use victory_hle::runtime::{forget_image, image_named, set_image_path};
        // Waits while another thread initialises a DLL; a `DllMain` that
        // loads another DLL re-enters.
        self.lock.hold(|| {
            if let Some(base) = image_named(file) {
                return Ok(Some(base));
            }
            let new = {
                let mut mapped = self.mapped.lock().unwrap_or_else(|e| e.into_inner());
                let wanted = |dll: &str| title_dll(&self.host, dll) && image_named(dll).is_none();
                let new = victory_loader::load_title_dll(
                    file,
                    &self.title_dir,
                    &self.cache_dir,
                    &wanted,
                )?;
                let hle = victory_hle::HleResolver::new(self.host.clone());
                let old = victory_loader::WithTitleDlls {
                    inner: &hle,
                    dlls: &mapped,
                };
                let resolver = victory_loader::WithTitleDlls {
                    inner: &old,
                    dlls: &new,
                };
                for (name, dll) in &new {
                    println!(
                        "title dll      {name} at {:#x} (loaded by the title)",
                        dll.base()
                    );
                    show(name, dll.bind_imports(&resolver, self.policy)?);
                    set_image_path(dll.base(), &self.title_dir.join(name));
                }
                // Visible before their `DllMain`s run, which may load more.
                mapped.extend(new.iter().cloned());
                new
            };
            let failed = new.iter().enumerate().find_map(|(i, (name, dll))| {
                // SAFETY: bound above, dependencies attached first.
                let attached = unsafe { dll.attach() };
                attached
                    .err()
                    .map(|e| (i, e.context(format!("attaching {name}"))))
            });
            let Some((i, error)) = failed else {
                return Ok(new.last().map(|(_, dll)| dll.base()));
            };
            // As `LoadLibrary` does when a `DllMain` fails: detach what this
            // load attached (the failed one too) and unmap all of it.
            let bases: Vec<_> = new.iter().map(|(_, d)| d.base()).collect();
            self.mapped
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(_, d)| !bases.contains(&d.base()));
            for (_, done) in new[..=i].iter().rev() {
                // SAFETY: attached (or failing to) on this thread just now.
                unsafe { done.detach() };
            }
            for (_, dll) in new {
                forget_image(dll.base());
                // SAFETY: detached, and no title code holds it yet.
                unsafe { dll.unload() };
            }
            Err(error)
        })
    }
}

/// The Windows loader lock's shape: one thread at a time, re-entered by
/// the thread that holds it.
#[cfg(windows)]
#[derive(Default)]
struct LoaderLock {
    owner: std::sync::Mutex<(Option<std::thread::ThreadId>, usize)>,
    free: std::sync::Condvar,
}

#[cfg(windows)]
impl LoaderLock {
    fn hold<T>(&self, f: impl FnOnce() -> T) -> T {
        let me = std::thread::current().id();
        let lock = || self.owner.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut owner = lock();
            while owner.0.is_some_and(|o| o != me) {
                owner = self.free.wait(owner).unwrap_or_else(|e| e.into_inner());
            }
            *owner = (Some(me), owner.1 + 1);
        }
        let result = f();
        let mut owner = lock();
        owner.1 -= 1;
        if owner.1 == 0 {
            owner.0 = None;
            self.free.notify_one();
        }
        result
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::sync::{Arc, Mutex};

    #[test]
    fn loader_lock_is_reentrant_and_exclusive() {
        let lock = Arc::new(super::LoaderLock::default());
        assert_eq!(lock.hold(|| lock.hold(|| 7)), 7);
        let order = Arc::new(Mutex::new(Vec::new()));
        let other = lock.hold(|| {
            let (theirs, their_order) = (lock.clone(), order.clone());
            let other = std::thread::spawn(move || {
                theirs.hold(|| their_order.lock().unwrap().push(2));
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            order.lock().unwrap().push(1);
            other
        });
        other.join().unwrap();
        assert_eq!(*order.lock().unwrap(), [1, 2]);
    }
}

#[cfg(not(windows))]
pub fn run(_: &Path, _: Unimplemented, _: bool, _: bool, _: Console) -> Result<()> {
    anyhow::bail!("running titles requires Windows")
}
