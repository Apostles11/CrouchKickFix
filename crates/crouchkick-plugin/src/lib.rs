//! FromWau.CrouchKickFix — native crouch-kick input buffer (rrplug port of FzzyMod's
//! `InputHooker`).
//!
//! Detours `inputsystem.dll`'s `PostEvent` and delays jump/crouch presses up to `BUFFER_MS`; if
//! the other lands within the window they re-emit in order (the crouch-kick). The per-frame flush
//! runs in rrplug's `runframe` (one detour, no separate `Update` hook).
//!
//! Jump/crouch are matched by ACTION (via the `tf2-input` crate, which reads the engine bind
//! table), so any rebind works. When a kick is detected, the speed gain is pushed to the
//! companion (`CKF_OnKick`) for the on-screen readout.
//!
//! PostEvent sig (Win64 __fastcall = extern "C"):
//!   u32 PostEvent(uintptr_t ctx, InputEventType_t nType, int nTick, ButtonCode_t scan, ButtonCode_t virt, int data3)

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crouchkick_core::{Btn, Buffer, Decision, Edge};
use retour::GenericDetour;
use rrplug::high::squirrel::call_sq_function;
use rrplug::mid::squirrel::{SQFUNCTIONS, SQVM_CLIENT};
use rrplug::prelude::*;
use tf2_input::Input;
use winapi::um::libloaderapi::GetModuleHandleA;

type PostEventFn = unsafe extern "C" fn(usize, i32, i32, i32, i32, i32) -> u32;

const POSTEVENT_RVA: usize = 0x7EC0; // inputsystem.dll
const INPUTCTX_RVA: usize = 0x69F40; // inputsystem.dll — input-system singleton (ctx arg for re-emit)
const IE_BUTTON_PRESSED: i32 = 0;
const IE_BUTTON_RELEASED: i32 = 1;

// Local-player velocity mirrors in client.dll (static prediction globals; verified live).
const VELX_RVA: usize = 0xB34C2C;
const VELY_RVA: usize = 0xB34C30;

// Local-player + wall-run state, RE'd from this client.dll (the registered GetLocalClientPlayer /
// IsWallRunning script functions). Source EHANDLE resolution:
//   handle  = u32 @ client.dll+0xC21658   (0xFFFFFFFF = no local player)
//   entlist = ptr @ client.dll+0xB0F030   (entries 0x20 bytes; serial @+0x10, obj ptr @+8)
//   wallrun = *(f32)(player + 0x249C) < 1.0   (exact check from IsWallRunning)
const LP_HANDLE_RVA: usize = 0xC21658;
const ENTLIST_RVA: usize = 0xB0F030;
const ENT_SERIAL_OFF: usize = 0x10;
const ENT_OBJ_OFF: usize = 0x8;
const WALLRUN_MEMBER_OFF: usize = 0x249C;

// After leaving the wall, capture this many frames before resolving the kick — lets the velocity
// redirect settle (outgoing speed) and catches a crouch/jump landing just after wall leave.
const POST_LEAVE_FRAMES: u32 = 5;

// A crouch kick's jump lands in the first few frames of contact. Still on the wall this many
// frames in with no jump => a sustained wallrun, not a kick: abandon the contact.
const MAX_KICK_WALL_FRAMES: u32 = 25;

// 启动阶段快速重试按键解析。
const BIND_RETRY_INTERVAL: u64 = 64;

// 连续失败较多次后降低刷新频率，避免再次产生周期性卡顿。
const BIND_SLOW_RETRY_INTERVAL: u64 = 3600;

// 前 20 次使用快速重试。
const BIND_FAST_RETRY_LIMIT: u32 = 20;

// Source 引擎 ButtonCode 的检查范围。
const BUTTON_CODE_SCAN_LIMIT: i32 = 256;

// 临时诊断开关。排查完成后改为 false。
const DEBUG_DETECTOR: bool = true;

static BINDS_READY: AtomicBool = AtomicBool::new(false);
static BIND_RETRY_COUNT: AtomicU32 = AtomicU32::new(0);
static NEXT_BIND_RETRY_TICK: AtomicU64 = AtomicU64::new(0);

// 记录上一帧状态，只在状态发生变化时输出日志。
static LAST_WR: AtomicBool = AtomicBool::new(false);
static LAST_JD: AtomicBool = AtomicBool::new(false);
static LAST_CD: AtomicBool = AtomicBool::new(false);


// 临时调试：进入地图后由 Rust 主动推送一次 HUD。
static DEBUG_FEEDBACK_SENT: AtomicBool = AtomicBool::new(false);
static DEBUG_PLAYER_FRAMES: AtomicU32 = AtomicU32::new(0);


// The buffer (the fix); pushed from the companion via CKF_SetOptions (ModSettings `ckf_enabled`).
static ENABLED: AtomicBool = AtomicBool::new(true);

/// Held state by action (crouch = either crouch bind type).
fn jump_down() -> bool {
    tf2_input::is_down(Input::Jump)
}
fn crouch_down() -> bool {
    tf2_input::is_down(Input::Crouch) || tf2_input::is_down(Input::ToggleCrouch)
}
/// Is this ButtonCode a jump / crouch key (for classifying buffered events)?
fn is_jump_key(scan: i32) -> bool {
    tf2_input::matches(Input::Jump, scan)
}
fn is_crouch_key(scan: i32) -> bool {
    tf2_input::matches(Input::Crouch, scan) || tf2_input::matches(Input::ToggleCrouch, scan)
}

/// 检查绑定表中是否确实存在：
/// 1. 至少一个 Jump 绑定；
/// 2. 至少一个 Crouch 或 ToggleCrouch 绑定。
///
/// 成功时返回对应的 ButtonCode。
fn find_required_binds() -> Option<(i32, i32)> {
    let mut jump_key: Option<i32> = None;
    let mut crouch_key: Option<i32> = None;

    for code in 0..BUTTON_CODE_SCAN_LIMIT {
        if jump_key.is_none()
            && tf2_input::matches(Input::Jump, code)
        {
            jump_key = Some(code);
        }

        if crouch_key.is_none()
            && (
                tf2_input::matches(Input::Crouch, code)
                || tf2_input::matches(Input::ToggleCrouch, code)
            )
        {
            crouch_key = Some(code);
        }

        if jump_key.is_some() && crouch_key.is_some() {
            break;
        }
    }

    match (jump_key, crouch_key) {
        (Some(jump), Some(crouch)) => Some((jump, crouch)),
        _ => None,
    }
}

// Consecutive frames currently wall-running (0 = not on wall; 1 = first wall frame).
static WALL_FRAMES: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy)]
struct RawEvent {
    ctx: usize,
    n_type: i32,
    n_tick: i32,
    scan: i32,
    virt: i32,
    data3: i32,
}

/// Per-wall-contact measurement. Opens on wall contact, accumulates jump/crouch across the whole
/// on-wall phase (however long), then captures POST_LEAVE_FRAMES after leaving before resolving —
/// so a crouch landing just after the jump (or just after leaving) is still caught, the redirect
/// settles into `outgoing`, and `jump_first_wf` reflects how soon the jump landed. Kicks REDIRECT
/// velocity rather than add raw speed, so we report incoming -> outgoing.
struct Contact {
    last_wf: u32,       // most recent wall-contact frame number seen
    jump_first_wf: u32, // wall-contact frame the jump first appeared (0 = not seen yet)
    crouch_seen: bool,  // crouch held at any point during the window
    incoming: f32,      // speed on the frame before contact started
    outgoing: f32,      // latest speed (final value = post-redirect outgoing speed)
    on_wall: bool,      // currently on the wall
    post: u32,          // frames elapsed since leaving the wall
}

struct State {
    buffer: Buffer,
    held: [[Option<RawEvent>; 2]; 2], // [Btn][Edge]
    contact: Option<Contact>,         // Some(..) while on a wall or in the post-leave capture
}

// Horizontal speed on the previous runframe (for capturing incoming speed at wall contact).
static PREV_SPEED: AtomicU32 = AtomicU32::new(0); // f32 bits
static TICK: AtomicU64 = AtomicU64::new(0); // runframe counter (bind-refresh throttle)

static DETOUR: OnceLock<GenericDetour<PostEventFn>> = OnceLock::new();
static STATE: OnceLock<Mutex<State>> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();
static INPUT_BASE: OnceLock<usize> = OnceLock::new();
static CLIENT_BASE: OnceLock<usize> = OnceLock::new();
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// client.dll base, cached once loaded.
fn client_base() -> Option<usize> {
    if let Some(b) = CLIENT_BASE.get() {
        return Some(*b);
    }
    let h = unsafe { GetModuleHandleA(c"client.dll".as_ptr()) };
    if h.is_null() {
        return None;
    }
    let b = h as usize;
    let _ = CLIENT_BASE.set(b);
    Some(b)
}

/// Local player's horizontal speed (u/s), or None if client.dll isn't loaded yet.
fn horiz_speed() -> Option<f32> {
    let b = client_base()?;
    unsafe {
        let vx = *((b + VELX_RVA) as *const f32);
        let vy = *((b + VELY_RVA) as *const f32);
        Some((vx * vx + vy * vy).sqrt())
    }
}

/// Resolve the local player's C_BasePlayer pointer via the Source EHANDLE table (mirrors
/// GetLocalClientPlayer's resolver). None if no valid local player.
fn local_player() -> Option<usize> {
    let c = client_base()?;
    unsafe {
        let handle = *((c + LP_HANDLE_RVA) as *const u32);
        if handle == 0xFFFF_FFFF {
            return None;
        }
        let idx = (handle & 0xFFFF) as usize;
        if idx >= 0x4000 {
            return None;
        }
        let serial = handle >> 16;
        let entlist = *((c + ENTLIST_RVA) as *const usize);
        if entlist == 0 {
            return None;
        }
        let entry = entlist + (idx << 5); // 0x20-byte entries
        if *((entry + ENT_SERIAL_OFF) as *const u32) != serial {
            return None;
        }
        let player = *((entry + ENT_OBJ_OFF) as *const usize);
        (player != 0).then_some(player)
    }
}

/// Whether the local player is wall-running — exact check from IsWallRunning: member < 1.0.
fn is_wallrunning() -> bool {
    local_player()
        .map(|p| unsafe { *((p + WALLRUN_MEMBER_OFF) as *const f32) < 1.0 })
        .unwrap_or(false)
}

fn now_ms() -> u64 {
    START.get().map(|s| s.elapsed().as_millis() as u64).unwrap_or(0)
}
fn reemit_ctx() -> usize {
    INPUT_BASE.get().copied().unwrap_or(0) + INPUTCTX_RVA
}
fn bi(b: Btn) -> usize {
    if b == Btn::Jump { 0 } else { 1 }
}
fn ei(e: Edge) -> usize {
    if e == Edge::Press { 0 } else { 1 }
}

/// Map an incoming event to (Btn, Edge) if it's a jump/crouch press/release.
fn classify(scan: i32, n_type: i32) -> Option<(Btn, Edge)> {
    let edge = match n_type {
        IE_BUTTON_PRESSED => Edge::Press,
        IE_BUTTON_RELEASED => Edge::Release,
        _ => return None,
    };
    if is_jump_key(scan) {
        Some((Btn::Jump, edge))
    } else if is_crouch_key(scan) {
        Some((Btn::Crouch, edge))
    } else {
        None
    }
}

unsafe extern "C" fn post_event_detour(
    a: usize,
    n_type: i32,
    n_tick: i32,
    scan: i32,
    virt: i32,
    data3: i32,
) -> u32 {
    let (Some(detour), Some(state_mx)) = (DETOUR.get(), STATE.get()) else {
        return 0;
    };
    let pass = |ctx: usize| unsafe { detour.call(ctx, n_type, n_tick, scan, virt, data3) };

    // Feed physical key state to tf2-input (it tracks held-state by ButtonCode; kick detection
    // reads is_down() by action in runframe — the press edges rarely coincide with the brief
    // wall contact, but the held state does).
    if n_type == IE_BUTTON_PRESSED || n_type == IE_BUTTON_RELEASED {
        tf2_input::on_button_event(scan, n_type == IE_BUTTON_PRESSED);
    }

    let Some((btn, edge)) = classify(scan, n_type) else {
        return pass(a); // not jump/crouch press/release -> untouched
    };

    if !ENABLED.load(Ordering::Relaxed) {
        return pass(a); // buffer disabled -> pass jump/crouch through untouched
    }

    enum Act {
        Pass,
        Swallow,
        FlushThenPass(RawEvent),
    }
    let act = {
        let mut st = state_mx.lock().unwrap();
        match st.buffer.on_event(btn, edge, now_ms()) {
            Decision::Pass => Act::Pass,
            Decision::Hold => {
                st.held[bi(btn)][ei(edge)] = Some(RawEvent { ctx: a, n_type, n_tick, scan, virt, data3 });
                Act::Swallow
            }
            Decision::FlushThenPass(ob, oe) => match st.held[bi(ob)][ei(oe)].take() {
                Some(h) => Act::FlushThenPass(h),
                None => Act::Pass,
            },
        }
    }; // lock released before any trampoline call

    match act {
        Act::Swallow => 0,
        Act::Pass => pass(reemit_ctx()),
        Act::FlushThenPass(h) => {
            unsafe { detour.call(h.ctx, h.n_type, h.n_tick, h.scan, h.virt, h.data3) };
            pass(reemit_ctx())
        }
    }
}

/// Per-frame: flush held events whose buffer window has elapsed.
fn flush() {
    let (Some(detour), Some(state_mx)) = (DETOUR.get(), STATE.get()) else {
        return;
    };
    let due: Vec<RawEvent> = {
        let mut st = state_mx.lock().unwrap();
        let now = now_ms();
        st.buffer
            .on_update(now)
            .into_iter()
            .filter_map(|(b, e)| st.held[bi(b)][ei(e)].take())
            .collect()
    };
    for h in due {
        unsafe { detour.call(h.ctx, h.n_type, h.n_tick, h.scan, h.virt, h.data3) };
    }
}

/// Install the PostEvent detour once inputsystem.dll is loaded. Returns true once resolved.
fn install_once() -> bool {
    if INSTALLED.load(Ordering::Acquire) {
        return true;
    }
    let h = unsafe { GetModuleHandleA(c"inputsystem.dll".as_ptr()) };
    if h.is_null() {
        return false; // not loaded yet; try next frame
    }
    let base = h as usize;
    let _ = INPUT_BASE.set(base);
    let target: PostEventFn = unsafe { std::mem::transmute(base + POSTEVENT_RVA) };
    let detour = match unsafe { GenericDetour::<PostEventFn>::new(target, post_event_detour) } {
        Ok(d) => d,
        Err(e) => {
            log::error!("PostEvent detour creation failed: {e}");
            INSTALLED.store(true, Ordering::Release); // don't retry forever
            return true;
        }
    };
    if DETOUR.set(detour).is_err() {
        return true;
    }
    if let Err(e) = unsafe { DETOUR.get().unwrap().enable() } {
        log::error!("PostEvent detour enable failed: {e}");
    } else {
        log::info!("crouch-kick: PostEvent detour installed");
    }
    INSTALLED.store(true, Ordering::Release);
    true
}

/// Companion pushes the ModSettings `ckf_enabled` toggle to the plugin.
#[rrplug::sqfunction(VM = "CLIENT", ExportName = "CKF_SetOptions")]
fn ckf_set_options(enabled: i32) {
    ENABLED.store(enabled != 0, Ordering::Relaxed);
}

/// Push a detected kick into the CLIENT VM by calling the companion's `CKF_OnKick`. Must run on
/// the engine thread (called from runframe). No-op if the client VM / function isn't ready.
fn push_kick(t: EngineToken, gain: i32, wall_frame: i32, crouch: bool) {
    let Some(sqvm) = *SQVM_CLIENT.get(t).borrow() else {
        log::warn!(
            "crouch-kick: feedback push failed — CLIENT Squirrel VM unavailable"
        );
        return;
    };

    let Some(sqfns) = SQFUNCTIONS.client.get() else {
        log::warn!(
            "crouch-kick: feedback push failed — CLIENT SQFUNCTIONS unavailable"
        );
        return;
    };

    log::info!(
        "crouch-kick: pushing feedback gain={} wall_frame={} crouch={}",
        gain,
        wall_frame,
        crouch
    );

    let result = call_sq_function::<(), _>(
        sqvm,
        sqfns,
        "CKF_OnKick",
        (gain, wall_frame, crouch as i32),
    );

    if result.is_err() {
        log::error!(
            "crouch-kick: CKF_OnKick Squirrel call failed"
        );
    } else {
        log::info!(
            "crouch-kick: CKF_OnKick Squirrel call succeeded"
        );
    }
}

pub struct CrouchKickFix;

impl Plugin for CrouchKickFix {
    const PLUGIN_INFO: PluginInfo = PluginInfo::new(
        c"FromWau.CrouchKickFix",
        c"CROUCHKCK", // log tag — exactly 9 chars
        c"CROUCHKICKFIX",
        PluginContext::CLIENT,
    );

    fn new(_reloaded: bool) -> Self {
        let _ = START.set(Instant::now());
        let _ = STATE.set(Mutex::new(State {
            buffer: Buffer::new(),
            held: [[None; 2]; 2],
            contact: None,
        }));
        register_sq_functions(ckf_set_options);
        Self
    }

    fn runframe(&self, t: EngineToken) {
        let tick = TICK.fetch_add(1, Ordering::Relaxed);
        if install_once() {
            flush();
        }

    // 只在进入地图、能够解析本地玩家之后读取绑定表。
    // refresh() 返回 true 后，还需要确认 Jump/Crouch 已实际解析。
    let player_available = local_player().is_some();

    if !BINDS_READY.load(Ordering::Relaxed)
        && player_available
    {
        let next_retry_tick =
            NEXT_BIND_RETRY_TICK.load(Ordering::Relaxed);

        if tick >= next_retry_tick {
            let attempt =
                BIND_RETRY_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

            let next_interval =
                if attempt < BIND_FAST_RETRY_LIMIT {
                    BIND_RETRY_INTERVAL
                } else {
                    BIND_SLOW_RETRY_INTERVAL
                };

            NEXT_BIND_RETRY_TICK.store(
                tick.saturating_add(next_interval),
                Ordering::Relaxed,
            );

            let table_readable = tf2_input::refresh();

            if table_readable {
                if let Some((jump_key, crouch_key)) =
                    find_required_binds()
                {
                    BINDS_READY.store(true, Ordering::Relaxed);

                    log::info!(
                        "crouch-kick: required binds resolved; \
                         jump_code={}, crouch_code={}, attempts={}",
                        jump_key,
                        crouch_key,
                        attempt
                    );
                } else if attempt <= 5 || attempt % 10 == 0 {
                    log::warn!(
                        "crouch-kick: bind table readable, \
                         but Jump/Crouch bindings are missing; \
                         attempt={}",
                        attempt
                    );
                }
            } else if attempt <= 5 || attempt % 10 == 0 {
                log::warn!(
                    "crouch-kick: input bind table not ready; \
                     attempt={}",
                    attempt
                );
            }
        }
    }


        // 临时测试 native DLL -> CLIENT Squirrel -> CKF_OnKick。
        // 检测到本地玩家后等待约 120 帧，再主动推送一次 +120 u/s。
        if local_player().is_some()
            && !DEBUG_FEEDBACK_SENT.load(Ordering::Relaxed)
        {
            let frames = DEBUG_PLAYER_FRAMES.fetch_add(1, Ordering::Relaxed) + 1;

            if frames >= 120
            {
                log::info!("debug: native feedback test");
        
                push_kick(t, 120, 1, true);

                DEBUG_FEEDBACK_SENT.store(true, Ordering::Relaxed);
            }
        }

        // Wall contact this frame, from the RE'd wall-run flag.
        let wr = is_wallrunning();
        let wf = if wr {
            WALL_FRAMES.fetch_add(1, Ordering::Relaxed) + 1 // 1 on the first wall frame
        } else {
            WALL_FRAMES.store(0, Ordering::Relaxed);
            0
        };

        let jd = jump_down();
        let cd = crouch_down();
        let spd = horiz_speed().unwrap_or(0.0);

        // 只在状态变化时记录，避免每帧刷日志。
        if DEBUG_DETECTOR {
            let old_wr = LAST_WR.swap(wr, Ordering::Relaxed);
            let old_jd = LAST_JD.swap(jd, Ordering::Relaxed);
            let old_cd = LAST_CD.swap(cd, Ordering::Relaxed);

            if old_wr != wr || old_jd != jd || old_cd != cd {
                log::info!(
                    "crouch-kick state changed: \
                     wallrun={}, wall_frames={}, \
                     jump={}, crouch={}, speed={:.1}, \
                     binds_ready={}",
                    wr,
                    wf,
                    jd,
                    cd,
                    spd,
                    BINDS_READY.load(Ordering::Relaxed)
                );
            }

            // 即使三个状态始终没有变化，也每 600 帧输出一次摘要。
            // 这样可以确认插件仍在运行，以及 local_player 是否可用。
            if tick % 600 == 0 {
                log::info!(
                    "crouch-kick diagnostic: \
                     local_player={}, binds_ready={}, \
                     wallrun={}, jump={}, crouch={}, speed={:.1}",
                    player_available,
                    BINDS_READY.load(Ordering::Relaxed),
                    wr,
                    jd,
                    cd,
                    spd
                );
            }
        }

        let prev = f32::from_bits(PREV_SPEED.load(Ordering::Relaxed));
        PREV_SPEED.store(spd.to_bits(), Ordering::Relaxed);

        // Kick detection: open a window on wall contact; accumulate jump/crouch across the on-wall
        // phase; after leaving, capture POST_LEAVE_FRAMES then resolve. `jump_first_wf` = how soon
        // the jump landed (1 = firstie); a long wallrun-then-jump is dropped (MAX_KICK_WALL_FRAMES).
        let mut kick_event: Option<(i32, i32, bool)> = None; // (gain, wall_frame, crouch)
        if let Some(state_mx) = STATE.get() {
            let mut st = state_mx.lock().unwrap();
            if wr && wf == 1 {
                st.contact = Some(Contact {
                    last_wf: 1,
                    jump_first_wf: if jd { 1 } else { 0 },
                    crouch_seen: cd,
                    incoming: prev,
                    outgoing: spd,
                    on_wall: true,
                    post: 0,
                });
            } else if let Some(c) = st.contact.as_mut() {
                if wr {
                    c.last_wf = wf;
                    c.on_wall = true;
                } else if c.on_wall {
                    c.on_wall = false; // first frame off the wall
                    c.post = 0;
                } else {
                    c.post += 1;
                }
                if jd && c.jump_first_wf == 0 {
                    c.jump_first_wf = c.last_wf;
                }
                c.crouch_seen |= cd;
                c.outgoing = spd;

                // Snapshot, ending the borrow of `c` before touching st.contact.
                let (on_wall, jfw, post) = (c.on_wall, c.jump_first_wf, c.post);

                if on_wall && jfw == 0 && wf > MAX_KICK_WALL_FRAMES {
                    st.contact = None; // sustained wallrun, not a kick — stop tracking
                } else if !on_wall && post >= POST_LEAVE_FRAMES {
                    let c = st.contact.take().unwrap();
                    if c.jump_first_wf > 0 {
                        let gain = (c.outgoing - c.incoming).round() as i32;
                        kick_event = Some((gain, c.jump_first_wf as i32, c.crouch_seen));
                    }
                    // else: pure wallrun, no jump — not a kick.
                }
            }
        }

        // Push the kick into Squirrel AFTER releasing the STATE lock (still on the engine thread).
        if let Some((gain, wall_frame, crouch)) = kick_event {
            if DEBUG_DETECTOR {
                log::info!(
                    "crouch-kick: kick detected; \
                     gain={}, wall_frame={}, crouch={}",
                    gain,
                    wall_frame,
                    crouch
                );
            }

            push_kick(t, gain, wall_frame, crouch);
        }
        }
    }
}

entry!(CrouchKickFix);
