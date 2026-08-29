//! Main loop: show pages, act on key presses, follow the state of the system,
//! reload the configuration when it changes, and reconnect after the deck has
//! been unplugged.

use crate::action;
use crate::config::{Action, Brightness, Button, Config, StateCase, Style};
use crate::device::{self, KeyEvent, StreamDeck};
use crate::icons;
use crate::render::Renderer;
use crate::state::{StatePoller, StateUpdate};
use crate::template;
use anyhow::{Context as _, Result};
use notify::{RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
/// Set by SIGHUP: re-read the configuration and keep the service running.
static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(sig: libc::c_int) {
    if sig == libc::SIGHUP {
        RELOAD.store(true, Ordering::SeqCst);
    } else {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }
}

fn install_signal_handlers() {
    let handler = on_signal as *const () as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGHUP, handler);
    }
}

/// What one key currently shows.
struct KeyView {
    /// Index of the active state case, 0 when the key has no state.
    case: usize,
    /// State the images were rendered from — a repeat needs no redraw.
    ctx: template::Context,
    normal: Vec<u8>,
    pressed: Option<Vec<u8>>,
}

/// The picture of the whole page.
struct PageView {
    keys: HashMap<u8, KeyView>,
    /// Image for keys that carry nothing.
    blank: Vec<u8>,
}

impl PageView {
    fn image(&self, key: u8, pressed: bool) -> &[u8] {
        match self.keys.get(&key) {
            Some(view) if pressed => view.pressed.as_ref().unwrap_or(&view.normal),
            Some(view) => &view.normal,
            None => &self.blank,
        }
    }
}

pub fn run(config_path: PathBuf) -> Result<()> {
    install_signal_handlers();

    let mut config = Config::load(&config_path)?;
    let watcher = watch_config(&config_path);
    let reload_rx = watcher.as_ref().map(|(_, rx)| rx);

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            return Ok(());
        }

        match serve(&mut config, &config_path, reload_rx) {
            Ok(Outcome::Shutdown) => return Ok(()),
            Ok(Outcome::Reconnect) => {}
            Err(e) => log::error!("{e:#}"),
        }

        // Device gone or an error: wait a moment, then try again.
        for _ in 0..20 {
            if SHUTDOWN.load(Ordering::SeqCst) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

enum Outcome {
    Shutdown,
    Reconnect,
}

fn serve(
    config: &mut Config,
    config_path: &Path,
    reload_rx: Option<&Receiver<()>>,
) -> Result<Outcome> {
    let Some((info, kind)) = device::find_devices(config.device.serial.as_deref())?
        .into_iter()
        .next()
    else {
        log::debug!("no Stream Deck found, waiting…");
        return Ok(Outcome::Reconnect);
    };

    let mut deck = StreamDeck::open(&info, kind)?;
    log::info!(
        "{} connected (serial {}, firmware {})",
        kind.name,
        deck.serial(),
        deck.firmware_version().unwrap_or_else(|_| "?".into())
    );

    deck.reset()?;
    let mut brightness = config.device.brightness.min(100);
    deck.set_brightness(brightness)?;

    let mut renderer = build_renderer(config, kind);
    let mut page_name = config.start_page();
    let mut history: Vec<String> = Vec::new();
    let mut view = build_page(config, &renderer, &page_name, kind.keys)?;
    show_page(&deck, &view, kind.keys)?;
    let mut poller = start_poller(config, &page_name);

    let mut last_reload = Instant::now();

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            let _ = deck.reset();
            return Ok(Outcome::Shutdown);
        }

        // Collect configuration changes, debounced.
        {
            let mut dirty = RELOAD.swap(false, Ordering::SeqCst);
            if let Some(rx) = reload_rx {
                while rx.try_recv().is_ok() {
                    dirty = true;
                }
            }
            if dirty && last_reload.elapsed() > Duration::from_millis(250) {
                last_reload = Instant::now();
                std::thread::sleep(Duration::from_millis(120)); // let the editor finish writing
                match Config::load(config_path) {
                    Ok(new_config) => {
                        log::info!("configuration reloaded");
                        *config = new_config;
                        renderer = build_renderer(config, kind);
                        if !config.pages.contains_key(&page_name) {
                            page_name = config.start_page();
                            history.clear();
                        }
                        brightness = config.device.brightness.min(100);
                        deck.set_brightness(brightness)?;
                        view = build_page(config, &renderer, &page_name, kind.keys)?;
                        show_page(&deck, &view, kind.keys)?;
                        poller = start_poller(config, &page_name);
                    }
                    Err(e) => log::error!("reload failed, keeping the previous version: {e:#}"),
                }
            }
        }

        // Redraw keys whose state changed.
        for update in poller.drain() {
            if let Err(e) = apply_state(&deck, &mut view, config, &renderer, &page_name, &update) {
                log::error!("page `{page_name}`, key {}: {e:#}", update.key);
            }
        }

        let events = match deck.poll_events(120) {
            Ok(events) => events,
            Err(e) => {
                log::warn!("connection lost: {e:#}");
                return Ok(Outcome::Reconnect);
            }
        };

        for event in events {
            match event {
                KeyEvent::Down(key) => {
                    let _ = deck.set_key_image(key, view.image(key, true));

                    let Some(action) = active_action(config, &page_name, &view, key) else {
                        continue;
                    };
                    match action {
                        Action::Run(cmd) => {
                            log::info!("key {key}: {cmd:?}");
                            if let Err(e) = action::run(&cmd) {
                                log::error!("{e:#}");
                            }
                            // The press likely changed what the state commands
                            // report, so ask them again right away.
                            poller.refresh();
                        }
                        Action::Page(target) => {
                            history.push(page_name.clone());
                            page_name = target;
                            view = build_page(config, &renderer, &page_name, kind.keys)?;
                            show_page(&deck, &view, kind.keys)?;
                            poller = start_poller(config, &page_name);
                        }
                        Action::Back => {
                            if let Some(previous) = history.pop() {
                                page_name = previous;
                                view = build_page(config, &renderer, &page_name, kind.keys)?;
                                show_page(&deck, &view, kind.keys)?;
                                poller = start_poller(config, &page_name);
                            }
                        }
                        Action::Brightness(value) => {
                            brightness = apply_brightness(brightness, &value);
                            deck.set_brightness(brightness)?;
                            log::info!("brightness: {brightness}%");
                        }
                        Action::Reload => match Config::load(config_path) {
                            Ok(new_config) => {
                                *config = new_config;
                                renderer = build_renderer(config, kind);
                                if !config.pages.contains_key(&page_name) {
                                    page_name = config.start_page();
                                    history.clear();
                                }
                                view = build_page(config, &renderer, &page_name, kind.keys)?;
                                show_page(&deck, &view, kind.keys)?;
                                poller = start_poller(config, &page_name);
                                log::info!("configuration reloaded");
                            }
                            Err(e) => log::error!("reload failed: {e:#}"),
                        },
                    }
                }
                KeyEvent::Up(key) => {
                    // After a page switch the image already belongs to the new
                    // page, so there is nothing to restore.
                    let _ = deck.set_key_image(key, view.image(key, false));
                }
            }
        }
    }
}

/// The action of the active state case, falling back to the key's own.
fn active_action(config: &Config, page_name: &str, view: &PageView, key: u8) -> Option<Action> {
    let button = config.pages.get(page_name)?.buttons.get(&key)?;
    let case = view
        .keys
        .get(&key)
        .and_then(|v| case_of(button, v.case))
        .and_then(|case| case.action.clone());
    case.or_else(|| button.action.clone())
}

fn case_of(button: &Button, index: usize) -> Option<&StateCase> {
    button.state.as_ref()?.cases.get(index)
}

fn build_renderer(config: &Config, kind: &device::Kind) -> Renderer {
    let font = config
        .defaults
        .font
        .as_deref()
        .map(|f| config.resolve_path(f));
    Renderer::new(kind.image_size, kind.rotate180, font.as_deref())
}

/// Watch every key on the page that has a `state` block.
fn start_poller(config: &Config, page_name: &str) -> StatePoller {
    let specs = config
        .pages
        .get(page_name)
        .map(|page| {
            page.buttons
                .iter()
                .filter_map(|(&key, button)| button.state.as_ref().map(|spec| (key, spec)))
                .collect()
        })
        .unwrap_or_default();
    StatePoller::start(specs)
}

/// Render every key of a page in its initial state.
fn build_page(config: &Config, renderer: &Renderer, page_name: &str, keys: u8) -> Result<PageView> {
    let page = config
        .pages
        .get(page_name)
        .with_context(|| format!("page `{page_name}` does not exist"))?;
    let page_style = page.style.merged(&config.defaults);

    let mut views = HashMap::new();
    for (&key, button) in &page.buttons {
        if key >= keys {
            log::warn!("page `{page_name}`: key {key} is outside the device ({keys} keys)");
            continue;
        }
        let ctx = template::Context::default();
        match render_key(config, renderer, &page_style, button, 0, &ctx) {
            Ok(view) => {
                views.insert(key, view);
            }
            Err(e) => {
                log::error!("page `{page_name}`, key {key}: {e:#}");
                views.insert(
                    key,
                    KeyView {
                        case: 0,
                        ctx,
                        normal: renderer.render_blank(&button.style.merged(&page_style))?,
                        pressed: None,
                    },
                );
            }
        }
    }

    Ok(PageView {
        keys: views,
        blank: renderer.render_blank(&page_style)?,
    })
}

/// Redraw one key after its state command reported something new.
fn apply_state(
    deck: &StreamDeck,
    view: &mut PageView,
    config: &Config,
    renderer: &Renderer,
    page_name: &str,
    update: &StateUpdate,
) -> Result<()> {
    let Some(page) = config.pages.get(page_name) else {
        return Ok(());
    };
    let Some(button) = page.buttons.get(&update.key) else {
        return Ok(());
    };

    let ctx = template::Context::from_output(&update.stdout, update.exit, update.case);
    if let Some(current) = view.keys.get(&update.key)
        && current.case == update.case
        && current.ctx == ctx
    {
        return Ok(()); // nothing changed
    }

    log::debug!(
        "key {}: state case {} — `{}`",
        update.key,
        update.case,
        update.stdout
    );

    let page_style = page.style.merged(&config.defaults);
    let rendered = render_key(config, renderer, &page_style, button, update.case, &ctx)?;
    deck.set_key_image(update.key, &rendered.normal)?;
    view.keys.insert(update.key, rendered);
    Ok(())
}

/// Render a key for `preview`. Without a device to ask, the catch-all case is
/// the best guess at the resting state; templates render empty.
pub fn preview_key(
    config: &Config,
    renderer: &Renderer,
    page_style: &Style,
    button: &Button,
) -> Result<Vec<u8>> {
    let case = button
        .state
        .as_ref()
        .and_then(|state| state.cases.iter().rposition(StateCase::is_catch_all))
        .unwrap_or(0);
    let ctx = template::Context::default();
    Ok(render_key(config, renderer, page_style, button, case, &ctx)?.normal)
}

/// Build one key image: the key's own look, overridden by the active state
/// case, with every piece of text run through the template engine.
fn render_key(
    config: &Config,
    renderer: &Renderer,
    page_style: &Style,
    button: &Button,
    case_index: usize,
    ctx: &template::Context,
) -> Result<KeyView> {
    let case = case_of(button, case_index);

    let style = match case {
        Some(case) => case.style.merged(&button.style).merged(page_style),
        None => button.style.merged(page_style),
    };
    let style = templated_style(&style, ctx);

    let label_source = case
        .and_then(|c| c.label.as_deref())
        .or(button.label.as_deref());
    let label = template::render_opt(label_source, ctx);

    let icon_source = case
        .and_then(|c| c.icon.as_deref())
        .or(button.icon.as_deref());
    let icon = icon_source
        .map(|raw| template::render(raw, ctx))
        .map(|raw| icons::parse(&raw, |path| config.resolve_path(path)));

    let normal = renderer.render(&style, label.as_deref(), icon.as_ref(), false)?;
    let pressed = if style.press_feedback.unwrap_or(true) {
        renderer
            .render(&style, label.as_deref(), icon.as_ref(), true)
            .ok()
    } else {
        None
    };

    Ok(KeyView {
        case: case_index,
        ctx: ctx.clone(),
        normal,
        pressed,
    })
}

/// Colours may be templated too, so a key can turn red on its own.
fn templated_style(style: &Style, ctx: &template::Context) -> Style {
    Style {
        background: template::render_opt(style.background.as_deref(), ctx),
        color: template::render_opt(style.color.as_deref(), ctx),
        icon_color: template::render_opt(style.icon_color.as_deref(), ctx),
        font: style.font.clone(),
        font_size: style.font_size,
        padding: style.padding,
        press_feedback: style.press_feedback,
    }
}

fn show_page(deck: &StreamDeck, view: &PageView, keys: u8) -> Result<()> {
    for key in 0..keys {
        deck.set_key_image(key, view.image(key, false))?;
    }
    Ok(())
}

fn apply_brightness(current: u8, value: &Brightness) -> u8 {
    match value {
        Brightness::Absolute(v) => (*v).clamp(0, 100) as u8,
        Brightness::Relative(raw) => {
            let trimmed = raw.trim();
            match trimmed.parse::<i16>() {
                Ok(delta) if trimmed.starts_with('+') || trimmed.starts_with('-') => {
                    (current as i16 + delta).clamp(0, 100) as u8
                }
                Ok(absolute) => absolute.clamp(0, 100) as u8,
                Err(_) => {
                    log::warn!("could not make sense of brightness value `{raw}`");
                    current
                }
            }
        }
    }
}

/// Watch the configuration directory, icons included, so a new image shows up
/// right away.
fn watch_config(config_path: &Path) -> Option<(notify::RecommendedWatcher, Receiver<()>)> {
    let dir = config_path.parent()?.to_path_buf();
    let (tx, rx) = channel();
    let notify_tx = Arc::new(tx);

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && event.kind.is_modify() | event.kind.is_create() | event.kind.is_remove()
        {
            let _ = notify_tx.send(());
        }
    })
    .map_err(|e| log::warn!("file watching unavailable: {e}"))
    .ok()?;

    watcher
        .watch(&dir, RecursiveMode::Recursive)
        .map_err(|e| log::warn!("cannot watch {}: {e}", dir.display()))
        .ok()?;

    log::debug!("watching {}", dir.display());
    Some((watcher, rx))
}
