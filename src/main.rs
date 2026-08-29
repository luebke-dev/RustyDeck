use anyhow::{Context, Result};
use rustydeck::{config, daemon, device};
use std::path::{Path, PathBuf};

const HELP: &str = "\
rustydeck — a Stream Deck service driven by a YAML file

USAGE:
  rustydeck [run]        Start the service (default)
  rustydeck devices      List attached Stream Decks
  rustydeck init         Write an example config to ~/.config/rustydeck
  rustydeck check        Validate the configuration without touching the device
  rustydeck preview      Render pages as PNG (check the layout without hardware)
  rustydeck icons [TEXT] Search the built-in Material Design Icons by name
  rustydeck install      Write and enable a systemd user unit
  rustydeck udev-rule    Print the udev rule for device access

OPTIONS:
  -c, --config <FILE>    Configuration file (default: ~/.config/rustydeck/config.yaml)
  -p, --page <NAME>      Page for `preview` (default: every page)
  -o, --out <FILE>       Output file for `preview` (default: <page>.png)
  -v, --verbose          More log output
  -h, --help             Show this help
";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut command = String::new();
    let mut config_path: Option<PathBuf> = None;
    let mut verbose = false;
    let mut page: Option<String> = None;
    let mut needle = String::new();
    let mut out: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("rustydeck {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-v" | "--verbose" => verbose = true,
            "-p" | "--page" => page = Some(args.next().context("--page needs a name")?),
            "-o" | "--out" => {
                out = Some(config::expand_tilde(
                    &args.next().context("--out needs a path")?,
                ))
            }
            "-c" | "--config" => {
                let value = args.next().context("--config needs a path")?;
                config_path = Some(config::expand_tilde(&value));
            }
            other if other.starts_with('-') => {
                anyhow::bail!("unknown option `{other}` — see `rustydeck --help`");
            }
            other if command.is_empty() => command = other.to_string(),
            // Anything after the command is its argument, e.g. `icons volume`.
            other => needle = other.to_string(),
        }
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(if verbose {
        "debug"
    } else {
        "info"
    }))
    .format_timestamp_secs()
    .init();

    let config_path = config_path.unwrap_or_else(config::Config::path);

    match command.as_str() {
        "" | "run" => {
            if !config_path.exists() {
                anyhow::bail!(
                    "{} does not exist — `rustydeck init` writes an example configuration",
                    config_path.display()
                );
            }
            daemon::run(config_path)
        }
        "devices" => list_devices(),
        "init" => init_config(&config_path),
        "check" => check_config(&config_path),
        "preview" => preview(&config_path, page.as_deref(), out.as_deref()),
        "icons" => list_icons(&needle),
        "install" => install_unit(&config_path),
        "udev-rule" => {
            print!("{}", udev_rule());
            Ok(())
        }
        other => anyhow::bail!("unknown command `{other}` — see `rustydeck --help`"),
    }
}

fn list_devices() -> Result<()> {
    let devices = device::find_devices(None)?;
    if devices.is_empty() {
        println!("No supported Stream Deck found.");
        println!("Plugged in and still nothing? Then the permissions on /dev/hidraw* are");
        println!("most likely missing — `rustydeck udev-rule` prints the matching rule.");
        return Ok(());
    }
    for (info, kind) in devices {
        print!(
            "{}\n  Path:     {}\n  USB ID:   {:04x}:{:04x}\n  Serial:   {}\n  Keys:     {} ({}x{}, {}x{} px)\n",
            kind.name,
            info.path.display(),
            info.vendor_id,
            info.product_id,
            info.serial,
            kind.keys,
            kind.cols,
            kind.rows,
            kind.image_size,
            kind.image_size,
        );
        match device::StreamDeck::open(&info, kind).and_then(|d| d.firmware_version()) {
            Ok(fw) => println!("  Firmware: {fw}"),
            Err(e) => println!("  Firmware: not readable ({e})"),
        }
    }
    Ok(())
}

/// Print icon names, optionally narrowed down by a search term.
fn list_icons(needle: &str) -> Result<()> {
    use rustydeck::icons;

    let names = icons::search(needle);
    if names.is_empty() {
        println!("No icon name contains `{needle}`.");
        return Ok(());
    }

    // Enough columns for the terminal, without pulling in a TTY crate.
    let width = names.iter().map(|n| n.len()).max().unwrap_or(20) + 2;
    let columns = (100 / width).max(1);
    for row in names.chunks(columns) {
        let line: String = row.iter().map(|n| format!("{n:width$}")).collect();
        println!("{}", line.trim_end());
    }

    if needle.is_empty() {
        println!(
            "\n{} icons (Material Design Icons {})",
            names.len(),
            icons::VERSION
        );
    } else {
        println!(
            "\n{} of {} icons match `{needle}`",
            names.len(),
            icons::count()
        );
    }
    println!("Use them as `icon: mdi:<name>` in the configuration.");
    Ok(())
}

fn check_config(path: &Path) -> Result<()> {
    let cfg = config::Config::load(path)?;
    println!("{} is fine.", path.display());
    println!("  Start page: {}", cfg.start_page());
    for (name, page) in &cfg.pages {
        println!("  Page `{name}`: {} keys in use", page.buttons.len());
        for (key, button) in &page.buttons {
            // Every icon the key can show: its own, plus one per state case.
            let icons_used = button.icon.iter().chain(
                button
                    .state
                    .iter()
                    .flat_map(|state| state.cases.iter())
                    .filter_map(|case| case.icon.as_ref()),
            );

            for raw in icons_used {
                // A templated icon only resolves at runtime.
                if rustydeck::template::is_template(raw) {
                    continue;
                }
                match rustydeck::icons::parse(raw, |path| cfg.resolve_path(path)) {
                    rustydeck::icons::IconRef::File(path) if !path.exists() => {
                        println!("    ! key {key}: missing icon file: {}", path.display());
                    }
                    rustydeck::icons::IconRef::Unknown(name) => {
                        let hints = rustydeck::icons::suggestions(&name);
                        print!("    ! key {key}: unknown icon `mdi:{name}`");
                        if hints.is_empty() {
                            println!();
                        } else {
                            println!(" — did you mean {}?", hints.join(", "));
                        }
                    }
                    _ => {}
                }
            }

            if let Some(state) = &button.state
                && !state.cases.iter().any(|case| case.is_catch_all())
            {
                println!(
                    "    ~ key {key}: no catch-all case — the key keeps its last look when \
                     nothing matches"
                );
            }
        }
    }
    Ok(())
}

/// Render pages into a PNG grid — handy for checking colours and labels
/// without plugging in the deck.
fn preview(config_path: &Path, page: Option<&str>, out: Option<&Path>) -> Result<()> {
    use rustydeck::render::Renderer;

    let cfg = config::Config::load(config_path)?;
    // Use the layout of the attached deck, falling back to the MK.2.
    let kind = device::find_devices(cfg.device.serial.as_deref())?
        .first()
        .map(|(_, k)| *k)
        .unwrap_or_else(|| device::kind_for(0x0080).expect("MK.2 ist bekannt"));

    let font = cfg.defaults.font.as_deref().map(|f| cfg.resolve_path(f));
    // No rotation for the preview: it should read right way up on screen.
    let renderer = Renderer::new(kind.image_size, false, font.as_deref());

    let pages: Vec<&String> = match page {
        Some(name) => vec![
            cfg.pages
                .keys()
                .find(|k| k.as_str() == name)
                .with_context(|| format!("page `{name}` does not exist"))?,
        ],
        None => cfg.pages.keys().collect(),
    };

    for name in pages {
        let page = &cfg.pages[name];
        let page_style = page.style.merged(&cfg.defaults);
        let gap = 8u32;
        let cell = kind.image_size + gap;
        let mut sheet = image::RgbImage::from_pixel(
            cell * kind.cols as u32 + gap,
            cell * kind.rows as u32 + gap,
            image::Rgb([0x0a, 0x0a, 0x0c]),
        );

        for key in 0..kind.keys {
            let jpeg = match page.buttons.get(&key) {
                Some(button) => {
                    rustydeck::daemon::preview_key(&cfg, &renderer, &page_style, button)?
                }
                None => renderer.render_blank(&page_style)?,
            };
            let tile = image::load_from_memory(&jpeg)?.to_rgb8();
            let x = gap + (key as u32 % kind.cols as u32) * cell;
            let y = gap + (key as u32 / kind.cols as u32) * cell;
            image::imageops::overlay(&mut sheet, &tile, x as i64, y as i64);
        }

        let path = match out {
            Some(p) => p.to_path_buf(),
            None => PathBuf::from(format!("{name}.png")),
        };
        sheet.save(&path)?;
        println!("preview of page `{name}`: {}", path.display());
    }
    Ok(())
}

fn init_config(path: &Path) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir.join("icons"))
        .with_context(|| format!("could not create {}", dir.display()))?;

    if path.exists() {
        println!("{} already exists — nothing overwritten.", path.display());
        return Ok(());
    }

    std::fs::write(path, EXAMPLE_CONFIG)
        .with_context(|| format!("could not write {}", path.display()))?;
    println!("example configuration written: {}", path.display());
    println!("icons belong in {}", dir.join("icons").display());
    println!("then run: rustydeck run");
    Ok(())
}

fn install_unit(config_path: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("cannot determine own path")?;
    let unit_dir = config::home().join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    let unit_path = unit_dir.join("rustydeck.service");

    let unit = format!(
        "[Unit]\n\
         Description=RustyDeck — Stream Deck service\n\
         Documentation=file://{config}\n\
         After=graphical-session.target\n\
         PartOf=graphical-session.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} --config {config} run\n\
         Restart=always\n\
         RestartSec=2\n\
         # So launched programs find the Wayland/X11 display, run\n\
         # `systemctl --user import-environment` once.\n\
         \n\
         [Install]\n\
         WantedBy=graphical-session.target\n",
        exe = exe.display(),
        config = config_path.display(),
    );

    std::fs::write(&unit_path, unit)
        .with_context(|| format!("{} is not writable", unit_path.display()))?;
    println!("unit written: {}", unit_path.display());
    println!("enable it with:");
    println!("  systemctl --user daemon-reload");
    println!("  systemctl --user enable --now rustydeck.service");
    Ok(())
}

fn udev_rule() -> String {
    let mut rule = String::from(
        "# /etc/udev/rules.d/70-streamdeck.rules\n\
         # Then: sudo udevadm control --reload-rules && sudo udevadm trigger\n\
         # (unplug the deck once and plug it back in)\n",
    );
    for kind in device::KINDS {
        rule.push_str(&format!(
            "SUBSYSTEM==\"hidraw\", ATTRS{{idVendor}}==\"{:04x}\", ATTRS{{idProduct}}==\"{:04x}\", TAG+=\"uaccess\"  # {}\n",
            device::ELGATO_VID,
            kind.product_id,
            kind.name,
        ));
    }
    rule
}

const EXAMPLE_CONFIG: &str = include_str!("../assets/example-config.yaml");
