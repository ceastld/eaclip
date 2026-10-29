//! Real App event-pump acceptance for three unrendered cache owners.
//! No HostState, database, clipboard, capture, hooks, listeners or input.
//! Debug Windows --features test-support; output must be a NEW absolute
//! repository .tmp/history-image-cache-... directory. This is retained-cache
//! accounting/owner handoff evidence, not an OS working-set or GPU paint test.

#[cfg(not(all(windows, debug_assertions, feature = "test-support")))]
fn main() {
    eprintln!("history_image_cache_pressure requires Windows Debug --features test-support");
    std::process::exit(2);
}

#[cfg(all(windows, debug_assertions, feature = "test-support"))]
fn main() -> anyhow::Result<()> { fixture::run() }

#[cfg(all(windows, debug_assertions, feature = "test-support"))]
#[allow(dead_code)]
#[path = "history_image_cache_fixture.rs"]
mod authored;

#[cfg(all(windows, debug_assertions, feature = "test-support"))]
mod fixture {
    use std::{fs, path::Path, sync::Arc, time::{Duration, Instant}};

    use anyhow::{anyhow, ensure, Context as _, Result};
    use cliphost_gpui::native_content::{
        self, budget::{shared_content_budget_snapshot, ContentBudgetSnapshot, SHARED_CONTENT_RETAINED_BYTES},
        cache::ListContentCache, model::{ContentMode, ContentProjection, MixedPart}, ContentImage, ContentResources,
    };
    use gpui::{div, prelude::*, App, Application, Context, Entity, Image, ImageFormat, Timer, Window, WindowOptions};
    use serde_json::{json, Value};

    use super::authored;

    const FOREGROUND: &str = "#ffffff";
    const LOCAL_LIMIT: usize = 32 * 1024 * 1024;
    const SIDE: u32 = 1400;

    struct Source { id: String, encoded: Vec<u8>, estimate: usize }

    fn projection(id: &str) -> ContentProjection {
        ContentProjection::Mixed(vec![MixedPart::Image { path:Some(format!("@owned-pressure:{id}")) }])
    }

    fn resource(bytes: &[u8]) -> ContentResources {
        let mut resources = ContentResources::default();
        resources.images.insert("owned-pressure-image".into(),Some(ContentImage {
            image:Arc::new(Image::from_bytes(ImageFormat::Png,bytes.to_vec())),
            width:SIDE as f32,height:SIDE as f32,render:None,
        }));
        resources
    }

    fn budget_json(snapshot: ContentBudgetSnapshot) -> Value {
        json!({"limitBytes":snapshot.limit_bytes,"retainedBytes":snapshot.retained_bytes,
            "entries":snapshot.entries,"pendingEvictions":snapshot.pending_evictions,
            "pendingEvictionBytes":snapshot.pending_eviction_bytes,
            "pressureNotificationPending":snapshot.pressure_notification_pending})
    }

    struct Owner {
        index: usize,
        cache: ListContentCache,
        sources: Vec<Source>,
        load_requests: usize,
        accepted: usize,
        rejected: usize,
        automatic_calls: usize,
        automatic_removed: usize,
    }

    impl Owner {
        fn new(index:usize,sources:Vec<Source>,cx:&mut Context<Self>) -> Self {
            native_content::register_cache_owner_for_test(cx,Self::drain_from_registry);
            let mut cache=ListContentCache::default(); // actual shared 64MiB
            cache.advance(1);
            cache.release_evicted(cx); // install the normal App retirement pumps
            let mut owner=Self { index,cache,sources,load_requests:0,accepted:0,rejected:0,automatic_calls:0,automatic_removed:0 };
            owner.publish_stable_working_set();
            owner
        }

        // This callback is passed only to the production weak-owner registry.
        // The fixture driver never calls it or mutates a foreign owner's cache.
        fn drain_from_registry(&mut self,cx:&mut Context<Self>) {
            self.automatic_calls+=1;
            let before=self.cache.len();
            self.cache.apply_budget_pressure();
            self.automatic_removed+=before-self.cache.len();
            self.cache.release_evicted(cx);
        }

        fn publish_stable_working_set(&mut self) {
            // Entities never render and the sole App window stays hidden. Keep
            // the same logical resource set to test suppression/convergence
            // without allowing viewport changes to rearm rejected pictures.
            self.cache.set_visible_items(self.sources.iter().map(|source|source.id.as_str()));
        }

        fn attempt(&mut self,slot:usize,cx:&mut Context<Self>) -> bool {
            self.publish_stable_working_set();
            let source=&self.sources[slot];
            let ticket=self.cache.reserve(1,&source.id,&projection(&source.id),ContentMode::Standard,FOREGROUND);
            let mut accepted=false;
            if let Some(ticket)=ticket {
                self.load_requests+=1;
                accepted=self.cache.finish(ticket,resource(&source.encoded),true);
                if accepted {self.accepted+=1;} else {self.rejected+=1;}
            }
            // Normal mutations release only this requesting owner's evictions.
            // When A is revoked by C, A is left untouched until the registry's
            // event callback runs; the driver gates retries on that handoff.
            self.cache.release_evicted(cx);
            accepted
        }

        fn present(&self,slot:usize) -> bool {
            let id=&self.sources[slot].id;
            self.cache.get(1,id,&projection(id),ContentMode::Standard,FOREGROUND).is_some()
        }

        fn report(&self) -> Value {
            json!({"owner":self.index,"rendered":false,"cacheEntries":self.cache.len(),
                "retainedBytes":self.cache.retained_bytes(),"pendingWorkers":self.cache.pending_len(),
                "present":[self.present(0),self.present(1)],"loadRequests":self.load_requests,
                "accepted":self.accepted,"rejected":self.rejected,
                "registryDrainCalls":self.automatic_calls,"registryRemovedEntries":self.automatic_removed,
                "sourceEstimates":self.sources.iter().map(|source|source.estimate).collect::<Vec<_>>()})
        }
    }

    enum Phase { WaitAutomaticDrain, StableWorkingSet, DropSettle, Finished }

    struct Driver {
        owners: Option<Vec<Entity<Owner>>>,
        phase: Phase,
        initial: Value,
        first_automatic: Option<Value>,
        stable_rounds: usize,
        halfway_loads: Option<usize>,
        final_owners: Option<Value>,
        settled_rounds: usize,
        peak_retained: usize,
    }

    impl Driver {
        fn reports(&self,cx:&App) -> Value {
            json!(self.owners.as_ref().unwrap().iter().map(|owner|owner.read(cx).report()).collect::<Vec<_>>())
        }

        fn loads(&self,cx:&App) -> usize {
            self.owners.as_ref().unwrap().iter().map(|owner|owner.read(cx).load_requests).sum()
        }

        fn start(groups:Vec<Vec<Source>>,cx:&mut App) -> Result<Self> {
            let baseline=shared_content_budget_snapshot();
            ensure!(baseline.limit_bytes==SHARED_CONTENT_RETAINED_BYTES && baseline.limit_bytes==64*1024*1024,"expected actual shared 64MiB budget");
            ensure!(baseline.retained_bytes==0 && baseline.entries==0,"independent App must start with an empty shared budget");
            let owners:Vec<_>=groups.into_iter().enumerate().map(|(index,sources)|cx.new(|cx|Owner::new(index,sources,cx))).collect();
            for index in 0..2 {
                for slot in 0..2 {
                    ensure!(owners[index].update(cx,|owner,cx|owner.attempt(slot,cx)),"initial four pictures must fit");
                }
                ensure!(owners[index].read(cx).cache.retained_bytes()<LOCAL_LIMIT,"owner exceeded local bound");
            }
            let before=shared_content_budget_snapshot();
            ensure!(before.entries==4 && before.pending_evictions==0,"initial FIFO order was not established");
            ensure!(owners.iter().all(|owner|owner.read(cx).automatic_calls==0),"registry drain ran before pressure trigger");
            let accepted=owners[2].update(cx,|owner,cx|owner.attempt(0,cx));
            let triggered=shared_content_budget_snapshot();
            ensure!(!accepted && triggered.pending_evictions>0,"third owner did not trigger actual FIFO pressure");
            ensure!(owners[0].read(cx).cache.len()==2,"foreign owner was manually drained before event handoff");
            ensure!(owners[0].read(cx).automatic_calls==0,"first pressure event unexpectedly executed synchronously");
            let initial=json!({"baseline":budget_json(baseline),"fourPictures":budget_json(before),
                "thirdOwnerRejected":!accepted,"triggered":budget_json(triggered),
                "oldestStillPhysicallyHeldUntilEventPump":owners[0].read(cx).cache.len()==2,
                "owners":owners.iter().map(|owner|owner.read(cx).report()).collect::<Vec<_>>()});
            Ok(Self {owners:Some(owners),phase:Phase::WaitAutomaticDrain,initial,first_automatic:None,
                stable_rounds:0,halfway_loads:None,final_owners:None,settled_rounds:0,peak_retained:triggered.retained_bytes})
        }

        fn tick(&mut self,cx:&mut App) -> Result<bool> {
            let budget=shared_content_budget_snapshot();
            self.peak_retained=self.peak_retained.max(budget.retained_bytes);
            ensure!(budget.retained_bytes<=budget.limit_bytes,"shared accounting exceeded 64MiB");
            match self.phase {
                Phase::WaitAutomaticDrain=>{
                    // Read-only polling is deliberate. No owner reserve,
                    // apply_budget_pressure or release_evicted is invoked here.
                    let owners=self.owners.as_ref().unwrap();
                    let oldest=owners[0].read(cx);
                    if oldest.automatic_removed==0 || budget.pending_evictions!=0 || budget.pressure_notification_pending {return Ok(false);}
                    ensure!(oldest.automatic_calls>0 && oldest.cache.len()==1,"registry did not physically drain hidden oldest owner");
                    ensure!(!oldest.present(0) && oldest.present(1),"FIFO removed the wrong entry or get revived it");
                    ensure!(budget.entries==3,"deferred retirement did not return the foreign charge");
                    ensure!(owners[1].read(cx).present(0) && owners[1].read(cx).present(1),"FIFO unexpectedly removed newer owner");
                    self.first_automatic=Some(json!({"budget":budget_json(budget),"owners":self.reports(cx),
                        "driverForeignDrains":0,"passed":true}));
                    self.phase=Phase::StableWorkingSet;
                }
                Phase::StableWorkingSet=>{
                    // Do not let a reserve's own pressure drain masquerade as
                    // the global handoff. A revoked charge must be serviced by
                    // the actual registry task before any owner is scheduled.
                    if budget.pending_evictions!=0 || budget.pressure_notification_pending {return Ok(false);}
                    for owner in self.owners.as_ref().unwrap() {
                        for slot in 0..2 {
                            owner.update(cx,|owner,cx|owner.attempt(slot,cx));
                            if shared_content_budget_snapshot().pending_evictions>0 {break;}
                        }
                        if shared_content_budget_snapshot().pending_evictions>0 {break;}
                        ensure!(owner.read(cx).cache.retained_bytes()<LOCAL_LIMIT,"owner exceeded local retained bound");
                    }
                    self.stable_rounds+=1;
                    if self.stable_rounds==16 {self.halfway_loads=Some(self.loads(cx));}
                    if self.stable_rounds>=32 {
                        let now=shared_content_budget_snapshot();
                        if now.pending_evictions!=0 || now.pressure_notification_pending {return Ok(false);}
                        let loads=self.loads(cx);
                        ensure!(loads==self.halfway_loads.context("missing midpoint count")?,"stable over-budget working set kept scheduling loads");
                        ensure!(loads<=18,"stable six-picture working set reloaded excessively: {loads}");
                        let owners=self.owners.as_ref().unwrap();
                        ensure!(!owners[0].read(cx).present(0),"oldest evicted row was revived without source/viewport change");
                        ensure!(owners.iter().any(|owner|owner.read(cx).automatic_removed>0),"no automatic owner drain evidence");
                        ensure!(owners.iter().all(|owner|owner.read(cx).cache.pending_len()==0),"physical worker slots leaked");
                        self.final_owners=Some(self.reports(cx));
                        // Drop all strong Entity owners. The registry only has
                        // weak handles; final cache/entity Drop must enqueue its
                        // own normal deferred retirement without a manual drain.
                        drop(self.owners.take());
                        self.phase=Phase::DropSettle;
                    }
                }
                Phase::DropSettle=>{
                    if budget.retained_bytes==0 && budget.entries==0 && budget.pending_evictions==0 && !budget.pressure_notification_pending {
                        self.settled_rounds+=1;
                        if self.settled_rounds>=3 {self.phase=Phase::Finished;return Ok(true);}
                    } else {self.settled_rounds=0;}
                }
                Phase::Finished=>return Ok(true),
            }
            Ok(false)
        }

        fn report(&self,status:&str,error:Option<&str>,elapsed_ms:u128,cx:&App) -> Value {
            json!({"schema":"clip.history-image-cache-pressure.v1","status":status,"error":error,
                "pid":std::process::id(),"elapsedMs":elapsed_ms,"initial":self.initial,
                "firstAutomaticForeignDrain":self.first_automatic,"stableRounds":self.stable_rounds,
                "midpointLoadRequests":self.halfway_loads,"finalOwnersBeforeDrop":self.final_owners,
                "liveOwners":self.owners.as_ref().map(|_|self.reports(cx)),
                "finalBudget":budget_json(shared_content_budget_snapshot()),"peakRetainedBytes":self.peak_retained,
                "ownerEntities":3,"sourcesPerOwner":2,"hiddenWindow":true,"cacheImagesRendered":false,
                "driverForeignDrains":0,"hostStarted":false,"databaseOpened":false,
                "clipboardAccessed":false,"capture":false,"globalHooks":false,"listeners":false,"inputInjected":false,
                "scope":"real foreground owner notification and deferred shared-budget retirement; retained estimate, not OS working-set or GPU allocation"})
        }
    }

    struct EmptyWindow;
    impl Render for EmptyWindow {
        fn render(&mut self,_window:&mut Window,_cx:&mut Context<Self>) -> impl IntoElement {div()}
    }

    pub fn run() -> Result<()> {
        let output=std::env::args_os().nth(1).context("pass one new absolute repository .tmp/history-image-cache-... directory")?;
        let output=Path::new(&output).to_path_buf();
        authored::safe_new_root(&output)?;
        let mut groups=Vec::new();
        for owner in 0..3 {
            let mut sources=Vec::new();
            for slot in 0..2 {
                let encoded=authored::png(SIDE,SIDE,(owner*61+slot*17+11) as u8)?;
                let estimate=resource(&encoded).retained_bytes();
                ensure!((14*1024*1024..16*1024*1024).contains(&estimate),"authored PNG retained estimate is not approximately 15MiB: {estimate}");
                sources.push(Source {id:format!("owned-pressure-{owner}-{slot}"),encoded,estimate});
            }
            ensure!(sources.iter().map(|source|source.estimate).sum::<usize>()<LOCAL_LIMIT,"two authored resources must fit one owner");
            groups.push(sources);
        }
        let total:usize=groups.iter().flatten().map(|source|source.estimate).sum();
        ensure!(total>SHARED_CONTENT_RETAINED_BYTES,"authored six-picture working set must exceed shared limit");
        fs::write(output.join("report.json"),serde_json::to_vec_pretty(&json!({"status":"starting","pid":std::process::id(),"requestedResourceEstimate":total,"clipboardAccessed":false,"hostStarted":false}))?)?;
        let started=Instant::now();
        Application::new().run(move |cx:&mut App| {
            cx.open_window(WindowOptions {show:false,focus:false,titlebar:None,..Default::default()},|_,cx|cx.new(|_|EmptyWindow)).expect("create one empty hidden owned event-pump window");
            let mut driver=match Driver::start(groups,cx) {
                Ok(driver)=>driver,
                Err(error)=>{
                    let report=json!({"schema":"clip.history-image-cache-pressure.v1","status":"failed",
                        "error":error.to_string(),"phase":"establish-fixture","pid":std::process::id(),
                        "finalBudget":budget_json(shared_content_budget_snapshot()),"hostStarted":false,
                        "databaseOpened":false,"clipboardAccessed":false,"hiddenWindow":true});
                    let _=fs::write(output.join("report.json"),serde_json::to_vec_pretty(&report).expect("serialize setup failure"));
                    eprintln!("pressure fixture setup failed: {error:#}");std::process::exit(1);
                }
            };
            cx.spawn(async move |cx| {
                loop {
                    Timer::after(Duration::from_millis(40)).await;
                    let outcome=cx.update(|cx| {
                        let result=if started.elapsed()>Duration::from_secs(20) {
                            Err(anyhow!("real owner event handoff/shared-budget convergence did not settle before deadline"))
                        } else {driver.tick(cx)};
                        let (status,error)=match &result {Ok(true)=>("passed",None),Ok(false)=>("running",None),Err(error)=>("failed",Some(error.to_string()))};
                        (result,driver.report(status,error.as_deref(),started.elapsed().as_millis(),cx))
                    });
                    match outcome {
                        Ok((result,report))=>{
                            if let Err(error)=fs::write(output.join("report.json"),serde_json::to_vec_pretty(&report).expect("serialize pressure receipt")) {
                                eprintln!("pressure receipt failed: {error}");std::process::exit(1);
                            }
                            match result {
                                Ok(false)=>continue,
                                Ok(true)=>{let _=cx.update(|cx|cx.quit());break;},
                                Err(error)=>{eprintln!("pressure probe failed: {error:#}");std::process::exit(1);}
                            }
                        }
                        Err(error)=>{eprintln!("pressure App pump unavailable: {error}");std::process::exit(1);}
                    }
                }
            }).detach();
        });
        Ok(())
    }
}
