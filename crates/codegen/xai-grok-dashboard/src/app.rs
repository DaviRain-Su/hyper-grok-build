//! Leptos SSR application.
//!
//! Every page and component is authored in Rust. Navigation and filter forms
//! use ordinary HTTP requests, so the dashboard has no JavaScript build step.

use std::sync::Arc;

use leptos::prelude::*;
use leptos_meta::provide_meta_context;
use leptos_router::components::{Route, Router, Routes};
use leptos_router::hooks::{use_params_map, use_query_map};
use leptos_router::path;

use crate::store::{
    ChatMessage, DashboardStore, EventKind, LogEntry, NamedMetric, ResourceMetrics, ServerMetrics,
    SessionCharts, SessionDetail, SessionMeta, SessionPage, SessionQuery, TimelineEvent,
};

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <div class="container">
                <header class="header">
                    <div class="brand">
                        <h1>"Hyper Observability"</h1>
                        <span>"local · read-only · Rust"</span>
                    </div>
                    <nav class="nav">
                        <a href="/">"Overview"</a>
                        <a href="/sessions">"Sessions"</a>
                        <a href="/resources">"Resources"</a>
                        <a href="/logs">"Logs"</a>
                        <a href="/api/sessions">"JSON API"</a>
                    </nav>
                </header>
                <main>
                    <Routes fallback=NotFoundPage>
                        <Route path=path!("/") view=OverviewPage/>
                        <Route path=path!("/sessions") view=SessionListPage/>
                        <Route path=path!("/sessions/:id") view=SessionDetailPage/>
                        <Route path=path!("/sessions/:id/timeline") view=TimelinePage/>
                        <Route path=path!("/sessions/:id/chat") view=ChatPage/>
                        <Route path=path!("/sessions/:id/charts") view=ChartsPage/>
                        <Route path=path!("/sessions/:id/live") view=LivePage/>
                        <Route path=path!("/resources") view=ResourcesPage/>
                        <Route path=path!("/logs") view=LogsPage/>
                    </Routes>
                </main>
            </div>
        </Router>
    }
}

#[component]
fn NotFoundPage() -> impl IntoView {
    view! {
        <div class="empty">
            <h2>"Page not found"</h2>
            <a href="/">"Return to overview"</a>
        </div>
    }
}

#[component]
fn OverviewPage() -> impl IntoView {
    let metrics = Resource::new(|| (), |_| fetch_server_metrics());
    let sessions = Resource::new(
        || (),
        |_| fetch_session_page(String::new(), String::new(), String::new(), 0, 12),
    );

    view! {
        <section>
            <h2>"Overview"</h2>
            <Suspense fallback=loading_cards>
                {move || metrics.get().map(render_overview_metrics)}
            </Suspense>
            <div class="grid">
                <div class="card">
                    <h3>"What this reads"</h3>
                    <p class="muted">"summary.json · signals.json · events.jsonl · chat_history.jsonl · unified.jsonl · memtrace"</p>
                </div>
                <div class="card">
                    <h3>"Privacy boundary"</h3>
                    <p class="muted">"The server only binds to loopback and never writes session data."</p>
                </div>
            </div>
            <h3>"Recent sessions"</h3>
            <Suspense fallback=loading_rows>
                {move || sessions.get().map(render_recent_sessions)}
            </Suspense>
        </section>
    }
}

fn render_overview_metrics(result: Result<ServerMetrics, ServerFnError>) -> AnyView {
    match result {
        Ok(metrics) => {
            let models = metrics.models.clone();
            view! {
                <div>
                    <div class="grid">
                        <MetricCard label="Sessions" value=metrics.total_sessions.to_string() accent="blue"/>
                        <MetricCard label="Active" value=metrics.active_sessions.to_string() accent="green"/>
                        <MetricCard label="Context tokens" value=format_number(metrics.total_tokens_used) accent="purple"/>
                        <MetricCard label="Tool calls" value=format_number(metrics.total_tool_calls) accent="blue"/>
                        <MetricCard label="Turns" value=format_number(metrics.total_turns) accent="green"/>
                        <MetricCard label="Errors" value=format_number(metrics.total_errors) accent="red"/>
                        <MetricCard label="Compactions" value=format_number(metrics.total_compactions) accent="yellow"/>
                        <MetricCard
                            label="Avg response"
                            value=metrics.avg_response_time_ms.map(format_millis).unwrap_or_else(|| "—".to_owned())
                            accent="blue"
                        />
                    </div>
                    <div class="card">
                        <h3>"Sessions by model"</h3>
                        <BarChart metrics=models unit=""/>
                    </div>
                </div>
            }
            .into_any()
        }
        Err(error) => error_view(error),
    }
}

fn render_recent_sessions(result: Result<SessionPage, ServerFnError>) -> AnyView {
    match result {
        Ok(page) if page.items.is_empty() => empty_view("No sessions found."),
        Ok(page) => view! {
            <div class="grid">
                {page.items.into_iter().map(|session| view! { <SessionCard session/> }).collect::<Vec<_>>()}
            </div>
        }
        .into_any(),
        Err(error) => error_view(error),
    }
}

#[component]
fn SessionListPage() -> impl IntoView {
    let query_map = use_query_map();
    let filters = Memo::new(move |_| {
        let map = query_map.get();
        (
            map.get("query").unwrap_or_default(),
            map.get("model").unwrap_or_default(),
            map.get("active").unwrap_or_default(),
        )
    });
    let sessions = Resource::new(
        move || filters.get(),
        |(query, model, active)| fetch_session_page(query, model, active, 0, 500),
    );

    view! {
        <section>
            <h2>"Sessions"</h2>
            <form class="filters" method="get" action="/sessions">
                <input name="query" type="search" placeholder="Search title, cwd, id…" value=move || filters.get().0/>
                <input name="model" type="search" placeholder="Model" value=move || filters.get().1/>
                <select name="active">
                    <option value="">"All states"</option>
                    <option value="true" selected=move || filters.get().2 == "true">"Active"</option>
                    <option value="false" selected=move || filters.get().2 == "false">"Inactive"</option>
                </select>
                <button type="submit">"Filter"</button>
                <a href="/sessions">"Clear"</a>
            </form>
            <Suspense fallback=loading_rows>
                {move || sessions.get().map(render_session_table)}
            </Suspense>
        </section>
    }
}

fn render_session_table(result: Result<SessionPage, ServerFnError>) -> AnyView {
    match result {
        Ok(page) if page.items.is_empty() => empty_view("No sessions match these filters."),
        Ok(page) => {
            let total = page.total;
            view! {
                <div>
                    <p class="muted">{format!("Showing {} of {total} sessions", page.items.len())}</p>
                    <div class="card table-wrap">
                        <table>
                            <thead><tr>
                                <th>"State"</th><th>"Session"</th><th>"Model"</th><th>"Agent"</th>
                                <th>"Turns"</th><th>"Tokens"</th><th>"Tools"</th><th>"Last active"</th>
                            </tr></thead>
                            <tbody>
                                {page.items.into_iter().map(|session| {
                                    let href = format!("/sessions/{}", session.id);
                                    let title = session.title.clone().unwrap_or_else(|| short_id(&session.id));
                                    view! {
                                        <tr>
                                            <td><StatusBadge active=session.is_active/></td>
                                            <td><a href=href>{title}</a><br/><span class="muted mono">{session.cwd}</span></td>
                                            <td>{session.model_id}</td>
                                            <td>{session.agent_name.unwrap_or_else(|| "—".to_owned())}</td>
                                            <td>{session.turn_count}</td>
                                            <td>{format_number(session.tokens_used)}</td>
                                            <td>{session.tool_call_count}</td>
                                            <td>{format_datetime(session.last_active_at)}</td>
                                        </tr>
                                    }
                                }).collect::<Vec<_>>()}
                            </tbody>
                        </table>
                    </div>
                </div>
            }
            .into_any()
        }
        Err(error) => error_view(error),
    }
}

#[component]
fn SessionDetailPage() -> impl IntoView {
    let id = session_id_memo();
    let detail = Resource::new(move || id.get(), fetch_session_detail);
    view! {
        <section>
            <Suspense fallback=loading_detail>
                {move || detail.get().map(|result| render_session_detail(result, id.get()))}
            </Suspense>
        </section>
    }
}

fn render_session_detail(
    result: Result<Option<SessionDetail>, ServerFnError>,
    session_id: String,
) -> AnyView {
    match result {
        Ok(Some(detail)) => {
            let meta = detail.meta.clone();
            let signals = detail.signals.unwrap_or_default();
            view! {
                <div>
                    <SessionHeading meta=meta.clone()/>
                    <SessionTabs id=session_id/>
                    <div class="grid">
                        <MetricCard label="Turns" value=signals.turn_count.to_string() accent="green"/>
                        <MetricCard label="Context tokens" value=format_number(signals.context_tokens_used) accent="blue"/>
                        <MetricCard label="Context use" value=format!("{}%", signals.context_window_usage) accent="purple"/>
                        <MetricCard label="Tool calls" value=signals.tool_call_count.to_string() accent="blue"/>
                        <MetricCard label="Errors" value=signals.error_count.to_string() accent="red"/>
                        <MetricCard label="Compactions" value=signals.compaction_count.to_string() accent="yellow"/>
                        <MetricCard label="Duration" value=format_duration(signals.session_duration_seconds) accent="green"/>
                        <MetricCard label="Peak RSS" value=signals.peak_rss_bytes.map(format_bytes).unwrap_or_else(|| "—".to_owned()) accent="purple"/>
                    </div>
                    <div class="card">
                        <h3>"Session metadata"</h3>
                        <table><tbody>
                            <InfoRow label="Session ID" value=meta.id/>
                            <InfoRow label="CWD" value=meta.cwd/>
                            <InfoRow label="Model" value=meta.model_id/>
                            <InfoRow label="Agent" value=meta.agent_name.unwrap_or_else(|| "—".to_owned())/>
                            <InfoRow label="Kind" value=meta.session_kind.unwrap_or_else(|| "top-level".to_owned())/>
                            <InfoRow label="Git branch" value=meta.git_branch.unwrap_or_else(|| "—".to_owned())/>
                            <InfoRow label="Created" value=format_datetime(meta.created_at)/>
                            <InfoRow label="Last active" value=format_datetime(meta.last_active_at)/>
                        </tbody></table>
                    </div>
                </div>
            }
            .into_any()
        }
        Ok(None) => empty_view("Session not found."),
        Err(error) => error_view(error),
    }
}

#[component]
fn TimelinePage() -> impl IntoView {
    let id = session_id_memo();
    let detail = Resource::new(move || id.get(), fetch_session_detail);
    let timeline = Resource::new(move || id.get(), |id| fetch_timeline(id, 2_000));
    view! {
        <section>
            <Suspense fallback=loading_detail>
                {move || detail.get().map(|result| render_heading_only(result, id.get()))}
            </Suspense>
            <SessionTabs id=move || id.get()/>
            <h3>"Event timeline"</h3>
            <Suspense fallback=loading_rows>
                {move || timeline.get().map(render_timeline)}
            </Suspense>
        </section>
    }
}

fn render_timeline(result: Result<Vec<TimelineEvent>, ServerFnError>) -> AnyView {
    match result {
        Ok(events) if events.is_empty() => empty_view("No timeline events were recorded."),
        Ok(events) => view! {
            <div class="timeline">
                {events.into_iter().map(|event| view! { <TimelineEventRow event/> }).collect::<Vec<_>>()}
            </div>
        }
        .into_any(),
        Err(error) => error_view(error),
    }
}

#[component]
fn ChatPage() -> impl IntoView {
    let id = session_id_memo();
    let detail = Resource::new(move || id.get(), fetch_session_detail);
    let chat = Resource::new(move || id.get(), |id| fetch_chat(id, 500));
    view! {
        <section>
            <Suspense fallback=loading_detail>
                {move || detail.get().map(|result| render_heading_only(result, id.get()))}
            </Suspense>
            <SessionTabs id=move || id.get()/>
            <h3>"Recent conversation"</h3>
            <Suspense fallback=loading_rows>
                {move || chat.get().map(render_chat)}
            </Suspense>
        </section>
    }
}

fn render_chat(result: Result<Vec<ChatMessage>, ServerFnError>) -> AnyView {
    match result {
        Ok(messages) if messages.is_empty() => empty_view("No chat history is available."),
        Ok(messages) => view! {
            <div>
                {messages.into_iter().map(|message| view! {
                    <article class="card chat-message">
                        <div class="chat-role">{message.role}</div>
                        <pre>{message.content}</pre>
                        {(!message.tool_calls.is_empty()).then(|| view! {
                            <div class="muted mono">{format!("Tools: {}", message.tool_calls.into_iter().map(|tool| tool.name).collect::<Vec<_>>().join(", "))}</div>
                        })}
                    </article>
                }).collect::<Vec<_>>()}
            </div>
        }
        .into_any(),
        Err(error) => error_view(error),
    }
}

#[component]
fn ChartsPage() -> impl IntoView {
    let id = session_id_memo();
    let detail = Resource::new(move || id.get(), fetch_session_detail);
    let charts = Resource::new(move || id.get(), fetch_session_charts);
    view! {
        <section>
            <Suspense fallback=loading_detail>
                {move || detail.get().map(|result| render_heading_only(result, id.get()))}
            </Suspense>
            <SessionTabs id=move || id.get()/>
            <Suspense fallback=loading_cards>
                {move || charts.get().map(render_charts)}
            </Suspense>
        </section>
    }
}

fn render_charts(result: Result<Option<SessionCharts>, ServerFnError>) -> AnyView {
    match result {
        Ok(Some(charts)) => {
            let event_counts = charts.event_counts.clone();
            let tool_duration = charts.tool_duration_ms.clone();
            let usage = charts.context_window_usage.min(100);
            view! {
                <div>
                    <div class="card">
                        <h3>"Context window"</h3>
                        <div class="progress"><span style=format!("width:{usage}%")></span></div>
                        <p class="muted">{format!("{} / {} tokens ({usage}%)", format_number(charts.context_tokens_used), format_number(charts.context_window_tokens))}</p>
                    </div>
                    <div class="grid">
                        <div class="card"><h3>"Event distribution"</h3><BarChart metrics=event_counts unit=""/></div>
                        <div class="card"><h3>"Tool time"</h3><BarChart metrics=tool_duration unit="ms"/></div>
                    </div>
                </div>
            }
            .into_any()
        }
        Ok(None) => empty_view("Session not found."),
        Err(error) => error_view(error),
    }
}

#[component]
fn LivePage() -> impl IntoView {
    let id = session_id_memo();
    let timeline = Resource::new(move || id.get(), |id| fetch_timeline(id, 100));
    view! {
        <meta http-equiv="refresh" content="2"/>
        <section>
            <h2>"Live session events"</h2>
            <SessionTabs id=move || id.get()/>
            <div class="card">
                <p>"This Rust-rendered page refreshes every two seconds."</p>
                <p class="muted mono">{move || format!("Raw SSE: /api/sessions/{}/events", id.get())}</p>
            </div>
            <Suspense fallback=loading_rows>
                {move || timeline.get().map(render_timeline)}
            </Suspense>
        </section>
    }
}

#[component]
fn ResourcesPage() -> impl IntoView {
    let resources = Resource::new(|| (), |_| fetch_resource_metrics());
    view! {
        <section>
            <h2>"Resources"</h2>
            <Suspense fallback=loading_cards>
                {move || resources.get().map(render_resources)}
            </Suspense>
        </section>
    }
}

fn render_resources(result: Result<ResourceMetrics, ServerFnError>) -> AnyView {
    match result {
        Ok(metrics) => view! {
            <div>
                <div class="grid">
                    <MetricCard label="Sessions storage" value=format_bytes(metrics.sessions_bytes) accent="blue"/>
                    <MetricCard label="Logs storage" value=format_bytes(metrics.logs_bytes) accent="yellow"/>
                    <MetricCard label="Memory traces" value=format_bytes(metrics.memtrace_bytes) accent="purple"/>
                    <MetricCard label="Active processes" value=metrics.processes.len().to_string() accent="green"/>
                </div>
                <div class="card"><p class="muted mono">{metrics.grok_home}</p></div>
                <div class="card table-wrap">
                    <table>
                        <thead><tr><th>"PID"</th><th>"Session"</th><th>"CWD"</th><th>"RSS"</th><th>"Footprint"</th><th>"Allocated"</th><th>"Sample"</th></tr></thead>
                        <tbody>
                            {metrics.processes.into_iter().map(|process| view! {
                                <tr>
                                    <td>{process.pid}</td>
                                    <td><a href=format!("/sessions/{}", process.session_id)>{short_id(&process.session_id)}</a></td>
                                    <td class="mono">{process.cwd}</td>
                                    <td>{process.rss_bytes.map(format_bytes).unwrap_or_else(|| "—".to_owned())}</td>
                                    <td>{process.footprint_bytes.map(format_bytes).unwrap_or_else(|| "—".to_owned())}</td>
                                    <td>{process.allocated_bytes.map(format_bytes).unwrap_or_else(|| "—".to_owned())}</td>
                                    <td>{process.sample_timestamp_ms.map(format_epoch_ms).unwrap_or_else(|| "—".to_owned())}</td>
                                </tr>
                            }).collect::<Vec<_>>()}
                        </tbody>
                    </table>
                </div>
            </div>
        }
        .into_any(),
        Err(error) => error_view(error),
    }
}

#[component]
fn LogsPage() -> impl IntoView {
    let query_map = use_query_map();
    let filters = Memo::new(move |_| {
        let map = query_map.get();
        (
            map.get("level").unwrap_or_default(),
            map.get("sessionId").unwrap_or_default(),
        )
    });
    let logs = Resource::new(
        move || filters.get(),
        |(level, session_id)| fetch_logs(level, session_id, 1_000),
    );
    view! {
        <section>
            <h2>"Unified logs"</h2>
            <form class="filters" method="get" action="/logs">
                <select name="level">
                    <option value="">"All levels"</option>
                    <option value="debug" selected=move || filters.get().0 == "debug">"Debug"</option>
                    <option value="info" selected=move || filters.get().0 == "info">"Info"</option>
                    <option value="warn" selected=move || filters.get().0 == "warn">"Warn"</option>
                    <option value="error" selected=move || filters.get().0 == "error">"Error"</option>
                </select>
                <input name="sessionId" placeholder="Session ID" value=move || filters.get().1/>
                <button type="submit">"Filter"</button>
                <a href="/logs">"Clear"</a>
            </form>
            <Suspense fallback=loading_rows>
                {move || logs.get().map(render_logs)}
            </Suspense>
        </section>
    }
}

fn render_logs(result: Result<Vec<LogEntry>, ServerFnError>) -> AnyView {
    match result {
        Ok(logs) if logs.is_empty() => empty_view("No matching log entries."),
        Ok(logs) => view! {
            <div class="card">
                {logs.into_iter().map(|entry| {
                    let class = format!("log-row {}", entry.level.to_ascii_lowercase());
                    let context = if entry.context.is_null() { String::new() } else { entry.context.to_string() };
                    view! {
                        <div class=class>
                            <span>{entry.timestamp}</span><span>{entry.level}</span><span>{entry.source}</span>
                            <span>{entry.message} " " <span class="muted">{context}</span></span>
                        </div>
                    }
                }).collect::<Vec<_>>()}
            </div>
        }
        .into_any(),
        Err(error) => error_view(error),
    }
}

#[component]
fn SessionHeading(meta: SessionMeta) -> impl IntoView {
    let title = meta.title.clone().unwrap_or_else(|| short_id(&meta.id));
    view! {
        <div class="header">
            <div><h2>{title}</h2><span class="muted mono">{meta.cwd}</span></div>
            <StatusBadge active=meta.is_active/>
        </div>
    }
}

fn render_heading_only(
    result: Result<Option<SessionDetail>, ServerFnError>,
    _session_id: String,
) -> AnyView {
    match result {
        Ok(Some(detail)) => view! { <SessionHeading meta=detail.meta/> }.into_any(),
        Ok(None) => empty_view("Session not found."),
        Err(error) => error_view(error),
    }
}

#[component]
fn SessionTabs(#[prop(into)] id: Signal<String>) -> impl IntoView {
    view! {
        <nav class="tabs">
            <a href=move || format!("/sessions/{}", id.get())>"Summary"</a>
            <a href=move || format!("/sessions/{}/timeline", id.get())>"Timeline"</a>
            <a href=move || format!("/sessions/{}/chat", id.get())>"Chat"</a>
            <a href=move || format!("/sessions/{}/charts", id.get())>"Charts"</a>
            <a href=move || format!("/sessions/{}/live", id.get())>"Live"</a>
            <a href=move || format!("/api/sessions/{}", id.get())>"JSON"</a>
        </nav>
    }
}

#[component]
fn SessionCard(session: SessionMeta) -> impl IntoView {
    let href = format!("/sessions/{}", session.id);
    let title = session
        .title
        .clone()
        .unwrap_or_else(|| short_id(&session.id));
    view! {
        <a class="card" href=href>
            <div style="display:flex;justify-content:space-between;gap:12px">
                <h3>{title}</h3><StatusBadge active=session.is_active/>
            </div>
            <p class="muted mono">{session.cwd}</p>
            <p class="muted">{session.model_id}</p>
            <div class="metrics">
                <SmallMetric label="Turns" value=session.turn_count.to_string()/>
                <SmallMetric label="Tokens" value=format_number(session.tokens_used)/>
                <SmallMetric label="Tools" value=session.tool_call_count.to_string()/>
            </div>
        </a>
    }
}

#[component]
fn StatusBadge(active: bool) -> impl IntoView {
    let class = if active { "badge active" } else { "badge" };
    let label = if active { "Active" } else { "Stored" };
    view! { <span class=class>{label}</span> }
}

#[component]
fn MetricCard(label: &'static str, value: String, accent: &'static str) -> impl IntoView {
    view! {
        <div class="card metric" style=format!("border-top:2px solid var(--{accent})")>
            <span class="metric-value">{value}</span><span class="metric-label">{label}</span>
        </div>
    }
}

#[component]
fn SmallMetric(label: &'static str, value: String) -> impl IntoView {
    view! { <span class="metric"><span class="metric-value">{value}</span><span class="metric-label">{label}</span></span> }
}

#[component]
fn InfoRow(label: &'static str, value: String) -> impl IntoView {
    view! { <tr><th>{label}</th><td class="mono">{value}</td></tr> }
}

#[component]
fn TimelineEventRow(event: TimelineEvent) -> impl IntoView {
    let class = match event.kind {
        EventKind::ToolStarted
        | EventKind::ToolCompleted
        | EventKind::McpToolCallStarted
        | EventKind::McpToolCallCompleted => "timeline-event tool",
        EventKind::TurnStarted | EventKind::TurnEnded => "timeline-event turn",
        EventKind::McpServerFailed
        | EventKind::McpTransportError
        | EventKind::GoalClassifierFailClosed => "timeline-event error",
        _ => "timeline-event",
    };
    let detail = event
        .tool_name
        .clone()
        .or(event.phase.clone())
        .or(event.outcome.clone())
        .unwrap_or_default();
    view! {
        <div class=class>
            <span>{event.ts.format("%H:%M:%S%.3f").to_string()}</span>
            <strong>{format!("{:?}", event.kind)}</strong>
            <span>{detail}</span>
            <span>{event.duration_ms.map(format_millis).unwrap_or_default()}</span>
        </div>
    }
}

#[component]
fn BarChart(metrics: Vec<NamedMetric>, unit: &'static str) -> impl IntoView {
    let metrics: Vec<_> = metrics.into_iter().take(12).collect();
    let max = metrics
        .iter()
        .map(|metric| metric.value)
        .max()
        .unwrap_or(1)
        .max(1);
    view! {
        <div class="chart">
            {metrics.into_iter().map(|metric| {
                let width = metric.value.saturating_mul(100) / max;
                view! {
                    <div class="bar-row">
                        <span title=metric.name.clone()>{metric.name.clone()}</span>
                        <div class="bar"><span style=format!("width:{width}%")></span></div>
                        <span class="mono">{format!("{}{}", format_number(metric.value), unit)}</span>
                    </div>
                }
            }).collect::<Vec<_>>()}
        </div>
    }
}

fn session_id_memo() -> Memo<String> {
    let params = use_params_map();
    Memo::new(move |_| params.get().get("id").unwrap_or_default())
}

fn loading_cards() -> AnyView {
    view! { <div class="grid"><div class="card muted">"Loading…"</div></div> }.into_any()
}

fn loading_rows() -> AnyView {
    view! { <div class="card muted">"Loading…"</div> }.into_any()
}

fn loading_detail() -> AnyView {
    view! { <div class="card muted">"Loading session…"</div> }.into_any()
}

fn error_view(error: ServerFnError) -> AnyView {
    let message = error.to_string();
    view! { <div class="error-box">{message}</div> }.into_any()
}

fn empty_view(message: &'static str) -> AnyView {
    view! { <div class="empty">{message}</div> }.into_any()
}

fn short_id(id: &str) -> String {
    id.chars().take(12).collect()
}

fn format_number(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!("{:.1}B", value as f64 / 1_000_000_000.0)
    } else if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * KIB;
    const GIB: f64 = 1024.0 * MIB;
    if bytes as f64 >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB)
    } else if bytes as f64 >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else if bytes as f64 >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_millis(milliseconds: u64) -> String {
    if milliseconds >= 1_000 {
        format!("{:.2}s", milliseconds as f64 / 1_000.0)
    } else {
        format!("{milliseconds}ms")
    }
}

fn format_datetime(datetime: chrono::DateTime<chrono::Utc>) -> String {
    datetime.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

fn format_epoch_ms(milliseconds: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(milliseconds as i64)
        .map(format_datetime)
        .unwrap_or_else(|| milliseconds.to_string())
}

#[server]
async fn fetch_server_metrics() -> Result<ServerMetrics, ServerFnError> {
    dashboard_store()?
        .server_metrics()
        .await
        .map_err(server_error)
}

#[server]
async fn fetch_session_page(
    query: String,
    model: String,
    active: String,
    offset: usize,
    limit: usize,
) -> Result<SessionPage, ServerFnError> {
    let active = match active.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    };
    dashboard_store()?
        .query_sessions(SessionQuery {
            query: nonempty(query),
            model: nonempty(model),
            active,
            offset,
            limit: Some(limit),
            ..Default::default()
        })
        .await
        .map_err(server_error)
}

#[server]
async fn fetch_session_detail(id: String) -> Result<Option<SessionDetail>, ServerFnError> {
    dashboard_store()?
        .get_session(&id)
        .await
        .map_err(server_error)
}

#[server]
async fn fetch_timeline(id: String, limit: usize) -> Result<Vec<TimelineEvent>, ServerFnError> {
    dashboard_store()?
        .get_timeline(&id, limit)
        .await
        .map_err(server_error)
}

#[server]
async fn fetch_chat(id: String, limit: usize) -> Result<Vec<ChatMessage>, ServerFnError> {
    dashboard_store()?
        .get_chat_history(&id, limit)
        .await
        .map_err(server_error)
}

#[server]
async fn fetch_session_charts(id: String) -> Result<Option<SessionCharts>, ServerFnError> {
    dashboard_store()?
        .session_charts(&id)
        .await
        .map_err(server_error)
}

#[server]
async fn fetch_resource_metrics() -> Result<ResourceMetrics, ServerFnError> {
    dashboard_store()?
        .resource_metrics()
        .await
        .map_err(server_error)
}

#[server]
async fn fetch_logs(
    level: String,
    session_id: String,
    limit: usize,
) -> Result<Vec<LogEntry>, ServerFnError> {
    dashboard_store()?
        .get_logs(
            limit,
            nonempty(level).as_deref(),
            nonempty(session_id).as_deref(),
        )
        .await
        .map_err(server_error)
}

fn dashboard_store() -> Result<Arc<DashboardStore>, ServerFnError> {
    use_context::<Arc<DashboardStore>>()
        .ok_or_else(|| ServerFnError::new("DashboardStore context is unavailable"))
}

fn server_error(error: anyhow::Error) -> ServerFnError {
    ServerFnError::new(error.to_string())
}

fn nonempty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}
