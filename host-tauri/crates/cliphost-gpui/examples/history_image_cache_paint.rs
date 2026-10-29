//! Real-paint cache retirement probe in one independent GPUI window.
//! No HostState/database/listener/clipboard/capture/hooks/input is initialized.
//! Run a Debug build with --features test-support and one NEW absolute
//! repository .tmp/history-image-cache-... output directory.

#[cfg(not(all(windows, debug_assertions, feature = "test-support")))]
fn main() {
    eprintln!("history_image_cache_paint requires Windows Debug --features test-support");
    std::process::exit(2);
}

#[cfg(all(windows, debug_assertions, feature = "test-support"))]
fn main() -> anyhow::Result<()> {
    fixture::run()
}

#[cfg(all(windows, debug_assertions, feature = "test-support"))]
#[allow(dead_code)]
#[path = "history_image_cache_fixture.rs"]
mod authored;

#[cfg(all(windows, debug_assertions, feature = "test-support"))]
mod fixture {
    use std::{fs, path::{Path, PathBuf}, sync::{Arc, Weak}, time::{Duration, Instant}};

    use anyhow::{anyhow, ensure, Context as _, Result};
    use cliphost_gpui::{
        history_image_resources::{load_path_resources, probe_source, PRIMARY_IMAGE_RESOURCE},
        native_content::{self, budget::ContentBudget, cache::ListContentCache, model::{ContentMode, ContentProjection, MixedPart}},
    };
    use gpui::{div, img, point, prelude::*, px, rgb, size, App, Application, Bounds, Context, Image, RenderImage, SpriteAtlasDiagnostics, Timer, Window, WindowBounds, WindowOptions};
    use serde_json::{json, Value};

    use super::authored;

    struct Spec {
        name: &'static str,
        path: PathBuf,
        version: String,
        frames: usize,
        // Worker-prepared display raster (Render source) instead of GPUI's
        // encoded-asset decode; retirement goes through the painted registry.
        display: bool,
    }

    const DISPLAY_HEIGHT: u32 = 160;

    struct Probe {
        specs: Vec<Spec>,
        budget: ContentBudget,
        cache: Option<ListContentCache>,
        projection: Option<ContentProjection>,
        index: usize,
        active_id: Option<u64>,
        source: Option<Weak<Image>>,
        rendered: Option<Weak<RenderImage>>,
        baseline: Option<SpriteAtlasDiagnostics>,
        paint_stats: Option<SpriteAtlasDiagnostics>,
        retiring: bool,
        rounds: Vec<Value>,
        frames_drawn: usize,
    }

    fn stats_json(stats: SpriteAtlasDiagnostics) -> Value {
        json!({"imageTiles":stats.image_tiles,"texturePages":stats.texture_pages,"textureBytes":stats.texture_bytes})
    }

    impl Probe {
        fn new(specs: Vec<Spec>) -> Self {
            Self { specs, budget: ContentBudget::new(32 * 1024 * 1024), cache: None, projection: None,
                index:0, active_id:None, source:None, rendered:None, baseline:None,
                paint_stats:None, retiring:false, rounds:vec![], frames_drawn:0 }
        }

        fn load_current(&mut self, cx: &mut Context<Self>) -> Result<()> {
            let spec = &self.specs[self.index % self.specs.len()];
            let projection = ContentProjection::Mixed(vec![MixedPart::Image { path:Some(format!("@owned-paint:{}:{}", spec.name, spec.version)) }]);
            let mut resources = load_path_resources(&spec.path, &spec.version).map_err(|error|anyhow!(error))?;
            if spec.display {
                let slot = resources.images.get_mut(PRIMARY_IMAGE_RESOURCE).context("primary slot missing")?;
                let image = slot.take().context("bounded authored image unavailable")?;
                let prepared = native_content::display_from_list_image(image, DISPLAY_HEIGHT);
                ensure!(prepared.render.is_some(), "{} display raster was not prepared", spec.name);
                *slot = Some(prepared);
            }
            let image = resources.images[PRIMARY_IMAGE_RESOURCE].as_ref().context("bounded authored image unavailable")?;
            self.active_id = Some(image.image.id());
            self.source = Some(Arc::downgrade(&image.image));
            let mut cache = ListContentCache::with_budget(self.budget.clone());
            cache.advance(1);
            let ticket = cache.reserve(1,"owned-painted-image",&projection,ContentMode::Standard,"#ffffff").context("cache reservation refused")?;
            ensure!(cache.finish(ticket,resources,true), "cache admission refused");
            cache.release_evicted(cx);
            ensure!(self.budget.snapshot().retained_bytes > 0, "cache did not retain a budget charge");
            self.cache = Some(cache);
            self.projection = Some(projection);
            self.rendered = None;
            self.retiring = false;
            cx.notify();
            Ok(())
        }

        fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Result<bool> {
            let stats = window.sprite_atlas_diagnostics().context("actual Windows atlas diagnostics unavailable")?;
            if self.baseline.is_none() {
                ensure!(stats.image_tiles == 0, "independent empty window unexpectedly contains image tiles");
                self.baseline = Some(stats);
                self.load_current(cx)?;
                return Ok(false);
            }
            let baseline = self.baseline.unwrap();
            if !self.retiring {
                let spec = &self.specs[self.index % self.specs.len()];
                let weak = if spec.display {
                    native_content::painted_render_weak(self.active_id.unwrap())
                } else {
                    native_content::rendered_image_weak(self.active_id.unwrap())
                };
                let Some(weak) = weak else { return Ok(false); };
                let Some(rendered) = weak.upgrade() else { return Ok(false); };
                // A registered weak asset alone does not prove GPU paint. Wait
                // for the actual atlas to contain a tile from this window.
                if stats.image_tiles <= baseline.image_tiles { return Ok(false); }
                ensure!(rendered.frame_count() == spec.frames, "{} frame count changed: {}",spec.name,rendered.frame_count());
                // Wait for every animation frame to reach the real atlas.
                // Decoding all frames while painting only frame zero would
                // miss both playback regressions and multi-tile retirement.
                if stats.image_tiles < baseline.image_tiles + spec.frames { return Ok(false); }
                if spec.frames > 1 {
                    for frame in 0..spec.frames {
                        ensure!(rendered.delay(frame) == image::Delay::from_numer_denom_ms(90,1), "{} frame delay changed",spec.name);
                        if frame > 0 {
                            ensure!(rendered.as_bytes(0) != rendered.as_bytes(frame), "{} decoded frames were flattened",spec.name);
                        }
                    }
                } else {
                    let dimensions = rendered.size(0);
                    let max_height = if spec.display { DISPLAY_HEIGHT as i32 } else { 640 };
                    ensure!(dimensions.width.0 <= 1024 && dimensions.height.0 <= max_height, "static GPU raster exceeded display bound");
                }
                self.rendered = Some(weak);
                self.paint_stats = Some(stats);
                drop(rendered);
                self.retiring = true;
                self.projection = None; // next frame paints the empty surface
                if self.index % 2 == 0 {
                    let cache = self.cache.as_mut().unwrap();
                    cache.advance(2);
                    cache.release_evicted(cx);
                } else {
                    // Last cache Drop has no App borrow. Its installed
                    // retirement handoff must free both asset and budget.
                    drop(self.cache.take());
                }
                cx.notify();
                return Ok(false);
            }
            let rendered_gone = self.rendered.as_ref().is_some_and(|weak|weak.upgrade().is_none());
            let source_gone = self.source.as_ref().is_some_and(|weak|weak.upgrade().is_none());
            let budget = self.budget.snapshot();
            let atlas_at_baseline = stats == baseline;
            if !(rendered_gone && source_gone && atlas_at_baseline && budget.retained_bytes == 0 && budget.entries == 0) {
                return Ok(false);
            }
            self.rounds.push(json!({"cycle":self.index,"source":self.specs[self.index % self.specs.len()].name,
                "retirement":if self.index % 2 == 0 {"advance+release_evicted"}else{"cacheDrop"},
                "decodedFrames":self.specs[self.index % self.specs.len()].frames,
                "displayRaster":self.specs[self.index % self.specs.len()].display,
                "actualPaintAtlas":stats_json(self.paint_stats.unwrap()),"settledAtlas":stats_json(stats),
                "sourceWeakExpired":source_gone,"renderImageWeakExpired":rendered_gone,
                "retainedBytes":budget.retained_bytes,"budgetEntries":budget.entries,"passed":true}));
            drop(self.cache.take());
            self.index += 1;
            // Two visits per authored image cover re-entry with identical
            // content Image ids after complete retirement.
            if self.index == self.specs.len() * 2 { return Ok(true); }
            self.load_current(cx)?;
            Ok(false)
        }

        fn report(&self, status: &str, error: Option<&str>, elapsed_ms: u128) -> Value {
            let budget = self.budget.snapshot();
            json!({"schema":"clip.history-image-cache-paint.v1","status":status,"error":error,
                "pid":std::process::id(),"elapsedMs":elapsed_ms,"framesDrawn":self.frames_drawn,
                "baselineAtlas":self.baseline.map(stats_json),"completedCycles":self.rounds,
                "activeIndex":self.index,"retiring":self.retiring,
                "budget":{"retainedBytes":budget.retained_bytes,"entries":budget.entries,"pendingEvictions":budget.pending_evictions},
                "hostStarted":false,"databaseOpened":false,"clipboardAccessed":false,
                "capture":false,"globalHooks":false,"listeners":false,"inputInjected":false,
                "scope":"one independent GPUI paint window; no product hide/show or OS working-set claim"})
        }
    }

    impl Render for Probe {
        fn render(&mut self, _window:&mut Window, _cx:&mut Context<Self>) -> impl IntoElement {
            self.frames_drawn += 1;
            let mut surface = div().size_full().bg(rgb(0x222222));
            if let (Some(cache),Some(projection)) = (&self.cache,&self.projection) {
                if let Some(image) = cache.get(1,"owned-painted-image",projection,ContentMode::Standard,"#ffffff")
                    .and_then(|resources|resources.images.get(PRIMARY_IMAGE_RESOURCE)).and_then(Option::as_ref) {
                    // GPUI requires an element id to persist animation state.
                    // Each visit gets its own state while Image ids deliberately
                    // repeat, exercising asset reuse after full retirement.
                    surface = surface.child(native_content::render_content_image(image,img(image.source())
                        .id(gpui::SharedString::from(format!("owned-paint-{}", self.index)))
                        .w(px(320.)).h(px(180.)).into_any_element()));
                }
            }
            surface
        }
    }

    pub fn run() -> Result<()> {
        let output = std::env::args_os().nth(1).context("pass one new absolute repository .tmp/history-image-cache-... output directory")?;
        let output = Path::new(&output).to_path_buf();
        authored::safe_new_root(&output)?;
        let mut specs = Vec::new();
        for (name,bytes,frames,display) in [
            ("large.png",authored::png(3840,2160,74)?,1,false),
            ("static.webp",authored::static_webp(3840,2160)?,1,false),
            ("animated.gif",authored::gif(320,180,4)?,4,false),
            ("animated.webp",authored::animated_webp(320,180)?,3,false),
            ("display.png",authored::png(3840,2160,75)?,1,true),
            ("display.webp",authored::static_webp(3840,2160)?,1,true),
        ] {
            let path = output.join(name);
            fs::write(&path,bytes)?;
            let version = probe_source(&path).map_err(|error|anyhow!(error))?.source_version;
            specs.push(Spec { name,path,version,frames,display });
        }
        fs::write(output.join("report.json"),serde_json::to_vec_pretty(&json!({"status":"starting","pid":std::process::id(),"clipboardAccessed":false,"hostStarted":false}))?)?;
        let started = Instant::now();
        Application::new().run(move |cx:&mut App| {
            let window = cx.open_window(WindowOptions {
                titlebar:None,focus:false,show:false,
                window_bounds:Some(WindowBounds::Windowed(Bounds::new(point(px(80.),px(80.)),size(px(360.),px(220.))))),
                ..Default::default()
            }, move |_,cx|cx.new(|_|Probe::new(specs))).expect("open independent owned paint probe");
            window.update(cx,|_,window,_| {
                cliphost_gpui::platform::set_no_activate(window,true).expect("mark owned probe no-activate");
                cliphost_gpui::platform::show_no_activate(window).expect("show owned probe without activating");
            }).expect("show owned probe");
            cx.spawn(async move |cx| {
                loop {
                    Timer::after(Duration::from_millis(80)).await;
                    let outcome = window.update(cx,|probe,window,cx| {
                        let result = if started.elapsed() > Duration::from_secs(30) {
                            Err(anyhow!("paint/asset/atlas/budget retirement did not settle before deadline"))
                        } else { probe.tick(window,cx) };
                        let (status,error) = match &result { Ok(true)=>("passed",None),Ok(false)=>("running",None),Err(error)=>("failed",Some(error.to_string())) };
                        let report = probe.report(status,error.as_deref(),started.elapsed().as_millis());
                        (result,report)
                    });
                    match outcome {
                        Ok((result,report)) => {
                            if let Err(error) = fs::write(output.join("report.json"),serde_json::to_vec_pretty(&report).expect("serialize probe receipt")) {
                                eprintln!("paint probe receipt failed: {error}");
                                std::process::exit(1);
                            }
                            match result {
                                Ok(false)=>continue,
                                Ok(true)=>{ let _=cx.update(|cx|cx.quit()); break; },
                                Err(error)=>{ eprintln!("paint probe failed: {error:#}"); std::process::exit(1); }
                            }
                        }
                        Err(error)=>{ eprintln!("owned paint probe window unavailable: {error}"); std::process::exit(1); }
                    }
                }
            }).detach();
        });
        Ok(())
    }
}
