//! Browser tests: components mounted in a real DOM (headless Firefox via
//! `wasm-bindgen-test-runner`), with real timers and the real server
//! function client. The seam is the browser's `fetch`: each test replaces it
//! with a Rust handler, so requests are encoded and replies decoded exactly
//! as in production, and no server-side code is involved.
//!
//! ```sh
//! cargo test -p gavmo --lib --features hydrate --target wasm32-unknown-unknown
//! ```

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use leptos::mount::mount_to;
use leptos::prelude::*;
use leptos::server_fn::ServerFn;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::wasm_bindgen_test;
use web_sys::HtmlElement;

use super::hooks::use_polling;
use super::tools::WorkspaceTools;
use super::workspace_modal::{ConsolePanel, StatsPanel, WorkspaceModal};
use crate::api;
use crate::types::{ConsoleChunk, ToolConfig, ToolList, VmStats, WorkspaceInfo, WorkspaceStatus};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

// --- Harness ---

/// Every request the fake `fetch` received: (server fn path, form body).
type Calls = Rc<RefCell<Vec<(String, String)>>>;

/// Replaces `globalThis.fetch` with `reply`, which maps a server function
/// path (e.g. `api::ListWorkspaces::PATH`) and its form-encoded arguments to
/// a JSON body, answered with HTTP 200. Returns the call log.
fn fake_fetch(reply: impl Fn(&str, &str) -> String + 'static) -> Calls {
    let calls = Calls::default();
    let log = Rc::clone(&calls);
    let reply = Rc::new(reply);
    let fetch = Closure::<dyn FnMut(web_sys::Request) -> js_sys::Promise>::new(
        move |request: web_sys::Request| {
            let url = request.url();
            let path = url
                .find("/api/")
                .and_then(|i| url.get(i..))
                .unwrap_or(&url)
                .to_owned();
            let body = request.text().unwrap();
            let log = Rc::clone(&log);
            let reply = Rc::clone(&reply);
            wasm_bindgen_futures::future_to_promise(async move {
                let body = JsFuture::from(body).await?.as_string().unwrap_or_default();
                let json = reply(&path, &body);
                log.borrow_mut().push((path, body));
                let headers = web_sys::Headers::new()?;
                headers.set("content-type", "application/json")?;
                let init = web_sys::ResponseInit::new();
                init.set_status(200);
                init.set_headers(&headers);
                Ok(web_sys::Response::new_with_opt_str_and_init(Some(&json), &init)?.into())
            })
        },
    );
    js_sys::Reflect::set(&js_sys::global(), &"fetch".into(), fetch.as_ref()).unwrap();
    // Lives as long as the page; the next test installs its own.
    fetch.forget();
    calls
}

fn calls_to(calls: &Calls, path: &str) -> Vec<String> {
    calls
        .borrow()
        .iter()
        .filter(|(p, _)| p == path)
        .map(|(_, body)| body.clone())
        .collect()
}

fn json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap()
}

async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .unwrap();
    });
    JsFuture::from(promise).await.unwrap();
}

/// Waits (up to 3s) for `cond`, letting effects, tasks and timers run.
async fn until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..300 {
        if cond() {
            return;
        }
        sleep(10).await;
    }
    panic!("timed out waiting for {what}");
}

/// A fresh container in the page's body to mount a component into.
fn container() -> HtmlElement {
    let document = web_sys::window().unwrap().document().unwrap();
    let div = document.create_element("div").unwrap();
    document.body().unwrap().append_child(&div).unwrap();
    div.unchecked_into()
}

fn text_of(root: &HtmlElement, selector: &str) -> Option<String> {
    root.query_selector(selector)
        .unwrap()
        .and_then(|el| el.text_content())
}

fn element(root: &HtmlElement, selector: &str) -> HtmlElement {
    root.query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("no {selector}"))
        .unchecked_into()
}

const ID: &str = "/home/user/repo";

// --- use_polling with real timers ---

/// A slow server (35 ms replies, 10 ms period) never sees overlapping
/// requests; turning polling off runs `on_stop`; unmounting stops the timer
/// and drops the reply still in flight.
#[wasm_bindgen_test]
async fn polling_never_overlaps_and_stops_on_unmount() {
    let started = Rc::new(Cell::new(0u32));
    let in_flight = Rc::new(Cell::new(0u32));
    let max_in_flight = Rc::new(Cell::new(0u32));
    let applied = Rc::new(Cell::new(0u32));
    let stopped = Rc::new(Cell::new(0u32));
    let active = RwSignal::new(true);

    let handle = {
        let (started, in_flight, max_in_flight, applied, stopped) = (
            Rc::clone(&started),
            Rc::clone(&in_flight),
            Rc::clone(&max_in_flight),
            Rc::clone(&applied),
            Rc::clone(&stopped),
        );
        mount_to(container(), move || {
            // `fetch` and `apply` must be `Copy`: share the counters through
            // stored values.
            let counters = StoredValue::new_local((started, in_flight, max_in_flight));
            let applied = StoredValue::new_local(applied);
            use_polling(
                move || active.get(),
                Duration::from_millis(10),
                move || {
                    let (started, in_flight, max) = counters.get_value();
                    async move {
                        started.set(started.get() + 1);
                        in_flight.set(in_flight.get() + 1);
                        max.set(max.get().max(in_flight.get()));
                        sleep(35).await;
                        in_flight.set(in_flight.get() - 1);
                        Some(())
                    }
                },
                move |()| applied.with_value(|a| a.set(a.get() + 1)),
                move || stopped.set(stopped.get() + 1),
                false,
            );
        })
    };

    until("several polls", || applied.get() >= 3).await;
    assert_eq!(max_in_flight.get(), 1, "requests overlapped");

    active.set(false);
    until("on_stop", || stopped.get() == 1).await;
    let after_stop = started.get();
    sleep(60).await;
    assert_eq!(started.get(), after_stop, "polled after stop");

    active.set(true);
    until("polling again", || started.get() > after_stop).await;
    drop(handle);
    let at_unmount = (started.get(), applied.get());
    sleep(80).await;
    assert_eq!(in_flight.get(), 0);
    assert_eq!(
        (started.get(), applied.get()),
        at_unmount,
        "a request started or a reply applied after unmount"
    );
}

// --- Console ---

/// While booting the console follows the log incrementally (passing back
/// `next_offset`), and the final fetch when booting ends is appended.
#[wasm_bindgen_test]
async fn the_console_appends_chunks_and_fetches_once_more_when_booting_ends() {
    let reads = Rc::new(Cell::new(0u32));
    let calls = {
        let reads = Rc::clone(&reads);
        fake_fetch(move |path, _| {
            assert_eq!(path, api::ReadConsoleLog::PATH);
            reads.set(reads.get() + 1);
            let chunk = match reads.get() {
                1 => ConsoleChunk {
                    text: "booting\n".to_owned(),
                    replace: true,
                    next_offset: 8,
                },
                2 => ConsoleChunk {
                    text: "ready\n".to_owned(),
                    replace: false,
                    next_offset: 14,
                },
                _ => ConsoleChunk {
                    text: String::new(),
                    replace: false,
                    next_offset: 14,
                },
            };
            json(&chunk)
        })
    };
    let status = RwSignal::new(WorkspaceStatus::Booting);
    let root = container();
    let _handle = mount_to(root.clone(), move || {
        let status = Memo::new(move |_| status.get());
        view! { <ConsolePanel id=StoredValue::new(ID.to_owned()) status /> }
    });

    until("the first chunk", || {
        text_of(&root, ".console-log").as_deref() == Some("booting\n")
    })
    .await;
    status.set(WorkspaceStatus::Running {
        url: "http://127.0.0.1:1".to_owned(),
    });
    until("the final chunk", || {
        text_of(&root, ".console-log").as_deref() == Some("booting\nready\n")
    })
    .await;

    let bodies = calls_to(&calls, api::ReadConsoleLog::PATH);
    assert!(bodies[0].contains("offset=0"), "{bodies:?}");
    assert!(bodies[1].contains("offset=8"), "{bodies:?}");
    sleep(1200).await;
    assert_eq!(reads.get(), 2, "no polling after booting ended");
    assert_eq!(
        text_of(&root, ".console-log").as_deref(),
        Some("booting\nready\n"),
        "the log stays visible after boot"
    );
}

// --- Stats ---

#[wasm_bindgen_test]
async fn stats_show_while_running_and_clear_when_it_stops() {
    fake_fetch(|path, _| {
        assert_eq!(path, api::PollVmStats::PATH);
        json(&Some(VmStats {
            cpu_percent: 50.0,
            vcpus: 2,
            mem_used_bytes: None,
            mem_total_bytes: None,
            uptime_secs: 61,
            disk_bytes: 1 << 30,
            host_port: 4242,
        }))
    });
    let status = RwSignal::new(WorkspaceStatus::Running {
        url: "http://127.0.0.1:1".to_owned(),
    });
    let root = container();
    let _handle = mount_to(root.clone(), move || {
        let status = Memo::new(move |_| status.get());
        view! { <StatsPanel id=StoredValue::new(ID.to_owned()) status /> }
    });

    until("stats", || {
        text_of(&root, ".stats-panel").is_some_and(|t| t.contains("4242"))
    })
    .await;
    status.set(WorkspaceStatus::Stopped);
    until("stats cleared", || text_of(&root, ".stats-panel").is_none()).await;
}

// --- Modal ---

/// Removing the workspace (here, or from another tab via the list refresh)
/// closes its modal, exactly once.
#[wasm_bindgen_test]
async fn the_modal_closes_once_when_its_workspace_disappears() {
    fake_fetch(|path, _| {
        assert_eq!(path, api::ListToolConfigs::PATH);
        json(&ToolList {
            tools: Vec::new(),
            config_error: None,
        })
    });
    let workspaces = RwSignal::new(vec![WorkspaceInfo {
        id: ID.to_owned(),
        path: ID.to_owned(),
        status: WorkspaceStatus::Stopped,
    }]);
    let closed = Arc::new(AtomicU32::new(0));
    let root = container();
    let _handle = {
        let closed = Arc::clone(&closed);
        mount_to(root.clone(), move || {
            let on_close = Callback::new(move |()| {
                closed.fetch_add(1, Ordering::SeqCst);
            });
            view! {
                <WorkspaceModal
                    id=ID.to_owned()
                    workspaces
                    on_changed=Callback::new(|()| {})
                    on_close
                />
            }
        })
    };

    until("the modal", || text_of(&root, ".modal h2").is_some()).await;
    assert_eq!(text_of(&root, ".modal h2").as_deref(), Some("repo"));
    sleep(30).await;
    assert_eq!(closed.load(Ordering::SeqCst), 0);

    workspaces.set(Vec::new());
    until("on_close", || closed.load(Ordering::SeqCst) == 1).await;
    workspaces.set(Vec::new());
    sleep(30).await;
    assert_eq!(closed.load(Ordering::SeqCst), 1, "on_close must run once");
}

// --- Tools ---

fn tool(configured: bool) -> ToolConfig {
    ToolConfig {
        id: "jira".to_owned(),
        description: "Jira".to_owned(),
        base_url: "https://h.example.com".to_owned(),
        auth: "bearer".to_owned(),
        secret_name: "s".to_owned(),
        instructions: None,
        configured,
        url_changed: false,
        bound_url: None,
        delivery: "a bearer token".to_owned(),
        approved_delivery: None,
    }
}

/// Saving reloads the list; the row updates in place, so its "Saved" result
/// survives the reload instead of vanishing with a re-created row.
#[wasm_bindgen_test]
async fn a_tool_row_keeps_saved_across_the_reload() {
    let saved = Rc::new(Cell::new(false));
    let calls = {
        let saved = Rc::clone(&saved);
        fake_fetch(move |path, _| {
            if path == api::SetToolSecret::PATH {
                saved.set(true);
                "null".to_owned()
            } else {
                assert_eq!(path, api::ListToolConfigs::PATH);
                json(&ToolList {
                    tools: vec![tool(saved.get())],
                    config_error: None,
                })
            }
        })
    };
    let root = container();
    let _handle = mount_to(root.clone(), move || {
        view! { <WorkspaceTools id=ID.to_owned() /> }
    });

    until("the tool row", || {
        text_of(&root, ".tool-status").as_deref() == Some("not configured")
    })
    .await;
    let input: web_sys::HtmlInputElement = element(&root, "input[type=password]").unchecked_into();
    input.set_value("tok-123");
    input
        .dispatch_event(&web_sys::Event::new("input").unwrap())
        .unwrap();
    element(&root, ".tool-actions button").click();

    until("the reload", || {
        text_of(&root, ".tool-status").as_deref() == Some("configured")
    })
    .await;
    assert_eq!(text_of(&root, ".tool-result--ok").as_deref(), Some("Saved"));
    assert_eq!(
        text_of(&root, ".tool-actions button").as_deref(),
        Some("Update")
    );
    assert_eq!(input.value(), "", "the token field is cleared after saving");
    let save_bodies = calls_to(&calls, api::SetToolSecret::PATH);
    assert_eq!(save_bodies.len(), 1);
    assert!(save_bodies[0].contains("token=tok-123"), "{save_bodies:?}");
    assert_eq!(calls_to(&calls, api::ListToolConfigs::PATH).len(), 2);
}
