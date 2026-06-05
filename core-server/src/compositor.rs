use std::collections::{HashSet, VecDeque};
use std::sync::{atomic::AtomicUsize, Arc};

use std::time::{Duration, Instant};
use windows::core::Interface;

use parking_lot::Mutex;
use tokio::time::MissedTickBehavior;

use windows::Win32::Graphics::CompositionSwapchain::{
    IPresentationBuffer, IPresentationManager, IPresentationSurface,
};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F};
use windows::Win32::Graphics::Direct2D::D2D1_INTERPOLATION_MODE_LINEAR;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

use crate::ipc::server::ServerState;
use crate::renderer::dcomp::{
    present_manager, AcquireOutcome, CompositeOutputResources, PresentOutcome, ACQUIRE_TIMEOUT_MS,
};
use crate::renderer::painter::D2DEngine;

const COMPOSITOR_TICK: Duration = Duration::from_micros(8_333);
const COMPOSITOR_DURATION_WINDOW: usize = 120;
const COMPOSITOR_DURATION_WARN_MS: u128 = 8;
const COMPOSITOR_WARN_COOLDOWN_TICKS: usize = 60;
const COMPOSITOR_METRICS_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
struct DirtyState {
    canvas_ids: HashSet<u32>,
    monitor_ids: HashSet<u32>,
}

impl DirtyState {
    fn is_empty(&self) -> bool {
        self.canvas_ids.is_empty() && self.monitor_ids.is_empty()
    }
}

/// Monitor-paced compositor dirty scheduler.
///
/// App `SubmitFrame` calls only mark dirty canvases after a successful World
/// present. The background compositor tick coalesces all marks so the same
/// MonitorScene is rendered at most once per tick, even when multiple Apps
/// submit frames in the same 16ms window.
pub struct CompositorScheduler {
    dirty: Mutex<DirtyState>,
}

impl CompositorScheduler {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            dirty: Mutex::new(DirtyState::default()),
        })
    }

    pub fn mark_canvas_presented(&self, canvas_id: u32) {
        self.dirty.lock().canvas_ids.insert(canvas_id);
    }

    pub fn mark_monitor_dirty(&self, monitor_id: u32) {
        self.dirty.lock().monitor_ids.insert(monitor_id);
    }

    fn take_dirty(&self) -> DirtyState {
        std::mem::take(&mut *self.dirty.lock())
    }
}

struct RenderTargetSnapshot {
    render_w: u32,
    render_h: u32,
    buffer: IPresentationBuffer,
    texture: ID3D11Texture2D,
    surface: IPresentationSurface,
    manager: IPresentationManager,
}

struct CompositeLayerSnapshot {
    canvas_id: u32,
    z_order: u32,
    x: i32,
    y: i32,
    logical_w: u32,
    logical_h: u32,
    source_textures: Vec<ID3D11Texture2D>,
    last_presented_idx: Arc<AtomicUsize>,
}

struct CompositeSceneSnapshot {
    monitor_id: u32,
    scene_id: u32,
    logical_w: u32,
    logical_h: u32,
    target: RenderTargetSnapshot,
    layers: Vec<CompositeLayerSnapshot>,
}

fn snapshot_monitor_target(
    resources: &CompositeOutputResources,
    idx: usize,
) -> Option<RenderTargetSnapshot> {
    Some(RenderTargetSnapshot {
        render_w: resources.render_w,
        render_h: resources.render_h,
        buffer: resources.buffers.get(idx)?.clone(),
        texture: resources.textures.get(idx)?.clone(),
        surface: resources.surface.clone(),
        manager: resources.manager.clone(),
    })
}

fn dirty_monitor_ids(state: &ServerState, dirty: &DirtyState) -> HashSet<u32> {
    let mut monitor_ids = dirty.monitor_ids.clone();
    if dirty.canvas_ids.is_empty() {
        return monitor_ids;
    }

    for (monitor_id, scene) in &state.monitor_scenes {
        if scene
            .layers
            .iter()
            .any(|layer| dirty.canvas_ids.contains(&layer.canvas_id))
        {
            monitor_ids.insert(*monitor_id);
        }
    }

    monitor_ids
}

fn snapshot_dirty_composite_scene_targets(
    state: &ServerState,
    dirty: &DirtyState,
) -> Vec<CompositeSceneSnapshot> {
    let mut scenes = Vec::new();

    for monitor_id in dirty_monitor_ids(state, dirty) {
        let Some(scene) = state.monitor_scenes.get(&monitor_id) else {
            continue;
        };
        let Some(output) = state.monitor_scene_outputs.get(&monitor_id) else {
            continue;
        };

        let target = match output.acquire_available_buffer(ACQUIRE_TIMEOUT_MS) {
            AcquireOutcome::Acquired(i) => snapshot_monitor_target(output, i),
            AcquireOutcome::TimedOut => None,
            AcquireOutcome::Failed(e) => {
                eprintln!(
                    "[compositor] monitor={} composite acquire failed: {}",
                    monitor_id, e
                );
                None
            }
        };
        let Some(target) = target else {
            continue;
        };

        let mut layers: Vec<_> = scene
            .layers
            .iter()
            .filter(|layer| layer.window.visible)
            .filter_map(|layer| {
                let canvas = state.canvases.get(&layer.canvas_id)?;
                Some(CompositeLayerSnapshot {
                    canvas_id: layer.canvas_id,
                    z_order: layer.z_order,
                    x: layer.window.x,
                    y: layer.window.y,
                    logical_w: layer.window.logical_w,
                    logical_h: layer.window.logical_h,
                    source_textures: canvas.resources.textures.clone(),
                    last_presented_idx: canvas.resources.last_presented_idx.clone(),
                })
            })
            .collect();
        layers.sort_by_key(|layer| layer.z_order);

        scenes.push(CompositeSceneSnapshot {
            monitor_id,
            scene_id: scene.monitor_id,
            logical_w: scene.metrics.logical_w,
            logical_h: scene.metrics.logical_h,
            target,
            layers,
        });
    }

    scenes
}

fn render_composite_scenes(d2d: &D2DEngine, scenes: &[CompositeSceneSnapshot]) {
    let live_source_keys: HashSet<usize> = scenes
        .iter()
        .flat_map(|scene| scene.layers.iter())
        .flat_map(|layer| layer.source_textures.iter())
        .map(|texture| texture.as_raw() as usize)
        .collect();
    d2d.prune_source_bitmap_cache(&live_source_keys);

    for scene in scenes {
        let Ok(target_bitmap) = d2d.get_target_bitmap(&scene.target.texture) else {
            eprintln!(
                "[compositor] monitor={} scene={} create target bitmap failed",
                scene.monitor_id, scene.scene_id
            );
            continue;
        };

        unsafe {
            d2d.dc.SetTarget(&target_bitmap);
            d2d.dc.BeginDraw();
            d2d.dc.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            d2d.dc
                .SetTransform(&windows::Foundation::Numerics::Matrix3x2 {
                    M11: scene.target.render_w as f32 / scene.logical_w.max(1) as f32,
                    M12: 0.0,
                    M21: 0.0,
                    M22: scene.target.render_h as f32 / scene.logical_h.max(1) as f32,
                    M31: 0.0,
                    M32: 0.0,
                });
        }

        for layer in &scene.layers {
            if layer.logical_w == 0 || layer.logical_h == 0 {
                continue;
            }
            let idx = layer
                .last_presented_idx
                .load(std::sync::atomic::Ordering::Relaxed);
            let Some(source_texture) = layer.source_textures.get(idx) else {
                continue;
            };
            let Ok(source_bitmap) = d2d.get_source_bitmap(source_texture) else {
                eprintln!(
                    "[compositor] monitor={} scene={} layer canvas={} get source bitmap failed",
                    scene.monitor_id, scene.scene_id, layer.canvas_id
                );
                continue;
            };
            let dst_rect = D2D_RECT_F {
                left: layer.x as f32,
                top: layer.y as f32,
                right: layer.x as f32 + layer.logical_w as f32,
                bottom: layer.y as f32 + layer.logical_h as f32,
            };
            unsafe {
                d2d.dc.DrawBitmap(
                    &source_bitmap,
                    Some(&dst_rect),
                    1.0,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    None,
                    None,
                );
            }
        }

        unsafe {
            let _ = d2d.dc.EndDraw(None, None);
            d2d.dc.SetTarget(None);
            match scene.target.surface.SetBuffer(&scene.target.buffer) {
                Err(e) => {
                    eprintln!(
                        "[compositor] monitor={} scene={} SetBuffer error: {}",
                        scene.monitor_id, scene.scene_id, e
                    );
                }
                Ok(()) => {
                    match present_manager(&scene.target.manager, "CompositeOutputResources") {
                        PresentOutcome::Success => {}
                        PresentOutcome::RetryNextTick => {}
                        PresentOutcome::DeviceLost => {
                            eprintln!(
                            "[compositor] monitor={} scene={} device-lost — output rebuild required (not yet implemented)",
                            scene.monitor_id, scene.scene_id
                        );
                        }
                    }
                }
            }
            while scene.target.manager.GetNextPresentStatistics().is_ok() {}
        }
    }
}

fn render_dirty_scenes(scheduler: &CompositorScheduler) -> usize {
    let dirty = scheduler.take_dirty();
    if dirty.is_empty() {
        return 0;
    }

    let snapshot = {
        let state = crate::ipc::server::SERVER_STATE.read();
        let scenes = snapshot_dirty_composite_scene_targets(&state, &dirty);
        if scenes.is_empty() {
            return 0;
        }
        (state.devices.clone(), scenes)
    };

    let (devices, scenes) = snapshot;
    let guard = devices.render_ctx.lock().unwrap();
    render_composite_scenes(&guard.d2d, &scenes);
    scenes.len()
}

fn record_compositor_duration(
    durations: &mut VecDeque<Duration>,
    sample: Duration,
) -> (Duration, bool) {
    durations.push_back(sample);
    while durations.len() > COMPOSITOR_DURATION_WINDOW {
        durations.pop_front();
    }
    if durations.is_empty() {
        return (Duration::ZERO, false);
    }

    let total_nanos: u128 = durations.iter().map(|d| d.as_nanos()).sum();
    let avg_nanos = total_nanos / durations.len() as u128;
    let avg = Duration::from_nanos(avg_nanos as u64);
    let warn = durations.len() == COMPOSITOR_DURATION_WINDOW
        && avg.as_millis() > COMPOSITOR_DURATION_WARN_MS;
    (avg, warn)
}

pub fn spawn_compositor_task(scheduler: Arc<CompositorScheduler>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(COMPOSITOR_TICK);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut durations = VecDeque::with_capacity(COMPOSITOR_DURATION_WINDOW);
        let mut warn_cooldown = 0usize;
        let mut metrics_started_at = Instant::now();
        let mut metrics_rendered_scenes = 0usize;

        loop {
            interval.tick().await;
            let tick_start = Instant::now();
            let rendered_scenes = render_dirty_scenes(&scheduler);
            metrics_rendered_scenes += rendered_scenes;

            if metrics_started_at.elapsed() >= COMPOSITOR_METRICS_INTERVAL {
                let elapsed = metrics_started_at.elapsed().as_secs_f64().max(0.001);
                if metrics_rendered_scenes > 0 {
                    eprintln!(
                        "[compositor] present rate: {:.0} scene/s over {:.2}s",
                        metrics_rendered_scenes as f64 / elapsed,
                        elapsed
                    );
                }
                metrics_started_at = Instant::now();
                metrics_rendered_scenes = 0;
            }

            if rendered_scenes == 0 {
                continue;
            }

            let (avg, warn) = record_compositor_duration(&mut durations, tick_start.elapsed());
            if warn && warn_cooldown == 0 {
                eprintln!(
                    "[compositor] avg composite duration over last {} ticks is {:.2}ms (last tick rendered {} scene(s))",
                    COMPOSITOR_DURATION_WINDOW,
                    avg.as_secs_f64() * 1000.0,
                    rendered_scenes
                );
                warn_cooldown = COMPOSITOR_WARN_COOLDOWN_TICKS;
            } else {
                warn_cooldown = warn_cooldown.saturating_sub(1);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_coalesces_duplicate_canvas_marks() {
        let scheduler = CompositorScheduler::new();
        scheduler.mark_canvas_presented(7);
        scheduler.mark_canvas_presented(7);
        scheduler.mark_canvas_presented(8);

        let dirty = scheduler.take_dirty();
        assert_eq!(dirty.canvas_ids.len(), 2);
        assert!(dirty.canvas_ids.contains(&7));
        assert!(dirty.canvas_ids.contains(&8));
        assert!(scheduler.take_dirty().is_empty());
    }

    #[test]
    fn scheduler_coalesces_duplicate_monitor_marks() {
        let scheduler = CompositorScheduler::new();
        scheduler.mark_monitor_dirty(1);
        scheduler.mark_monitor_dirty(1);
        scheduler.mark_monitor_dirty(2);

        let dirty = scheduler.take_dirty();
        assert_eq!(dirty.monitor_ids.len(), 2);
        assert!(dirty.monitor_ids.contains(&1));
        assert!(dirty.monitor_ids.contains(&2));
        assert!(scheduler.take_dirty().is_empty());
    }
}
