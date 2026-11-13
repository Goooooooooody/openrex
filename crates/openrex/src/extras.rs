//! The rest of the server's API that a person can use from the window:
//! other clients' actions (`client.list`, `client.inspect`, `client.perform`),
//! any block's methods (`session.describe_block`), block types
//! (`block_creator.list`), detached panes (`session.new_block`), the session's
//! clients, sending a floating pane back (`session.lower_layer`), the client's
//! label, Tailscale sign-out and rename, and looking up sessions by any name
//! (`target.resolve`).

use gpui::{Context, Window};
use serde_json::{Value, json};

use crate::actions::{Outcome, Target};
use crate::palette::{Item, Palette};
use crate::store::{active_window, focused_block, shell_layout_in};
use crate::workspace::Workspace;

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn ok() -> Option<Outcome> {
    Some(Ok(Value::Null))
}

fn conflict(message: &str) -> Option<Outcome> {
    Some(Err(("conflict".into(), message.into())))
}

pub(crate) fn perform(
    ws: &mut Workspace,
    action: &str,
    args: &Value,
    target: &Target,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Outcome> {
    match action {
        "client.other.perform" => Some(other_client(ws, args, cx)),
        "pane.methods" => Some(block_methods(ws, args, target, cx)),
        "client.tab.new_with" => match arg(args, "flavor") {
            Some(flavor) => {
                let layout = json!({"block": {"flavor": flavor}});
                Some(ws.session(cx, "session.new_window", json!({"layout": layout})))
            }
            None => Some(block_types(ws, cx)),
        },
        "pane.new_detached" => {
            let cwd = ws.focused_cwd(cx);
            let options = shell_layout_in(cwd)["block"]["options"].clone();
            let mut payload = json!({"flavor": crate::store::SHELL_FLAVOR});
            if !options.is_null() {
                payload["options"] = options;
            }
            ws.store.update(cx, |s, cx| {
                let Some(session) = s.current.clone() else { return };
                payload["session_id"] = json!(session);
                s.call("session.new_block", payload, cx, |s, r, _| match r {
                    Ok(_) => s.notice = Some("Started a detached pane. Show Detached Pane… places it.".into()),
                    Err(e) => s.error = Some(format!("session.new_block: {}", e.message)),
                });
            });
            ok()
        }
        "pane.lower_floating" => {
            let view = ws.view(cx)?;
            let win = active_window(&view)?;
            let focused = win.focused_block_id.clone();
            let layer = win
                .layers
                .iter()
                .filter(|l| l.is_floating())
                .find(|l| l.blocks.iter().any(|b| b.block_id == focused))
                .and_then(|l| l.layer_id.clone());
            match layer {
                Some(id) => Some(ws.session(cx, "session.lower_layer", json!({"layer_id": id}))),
                None => conflict("the focused pane isn't floating"),
            }
        }
        "session.clients" => Some(session_clients(ws, cx)),
        "client.set_label" => match args.get("label").and_then(Value::as_str) {
            Some(label) => {
                let label = label.to_string();
                ws.store.update(cx, |s, cx| {
                    s.call("client.set_label", json!({"label": label}), cx, move |s, r, _| match r {
                        Ok(_) if label.is_empty() => s.notice = Some("Removed this client's label".into()),
                        Ok(_) => s.notice = Some(format!("This client is now {label}: rex -C {label} …")),
                        Err(e) => s.error = Some(format!("client.set_label: {}", e.message)),
                    });
                });
                ok()
            }
            None => {
                ws.palette = Some(
                    Palette::input("Set Client Label", "client.set_label", json!({}), "label", "")
                        .placeholder("A name other clients can use, as in rex -C NAME. Empty removes it."),
                );
                ok()
            }
        },
        "client.tailscale.logout" => {
            ws.store.update(cx, |s, cx| {
                s.call("server.tailscale.logout", json!({}), cx, |s, r, _| match r {
                    Ok(_) => s.notice = Some("Signed the server out of Tailscale".into()),
                    Err(e) => s.error = Some(format!("server.tailscale.logout: {}", e.message)),
                });
            });
            ok()
        }
        "client.tailscale.rename" => match arg(args, "hostname") {
            Some(name) => {
                let name = name.to_string();
                ws.store.update(cx, |s, cx| {
                    s.call("server.tailscale.rename", json!({"hostname": name}), cx, move |s, r, _| match r {
                        Ok(_) => s.notice = Some(format!("The server's Tailscale name is now {name}")),
                        Err(e) => s.error = Some(format!("server.tailscale.rename: {}", e.message)),
                    });
                });
                ok()
            }
            None => {
                ws.palette = Some(
                    Palette::input("Rename on Tailscale", "client.tailscale.rename", json!({}), "hostname", "")
                        .placeholder("Letters, digits, and hyphens, such as studio"),
                );
                ok()
            }
        },
        _ => None,
    }
}

/// Run Action on Another Client: pick a client, then one of its actions.
fn other_client(ws: &mut Workspace, args: &Value, cx: &mut Context<Workspace>) -> Outcome {
    let Some(control) = ws.store.read(cx).control() else { return Ok(Value::Null) };
    let me = ws.store.read(cx).client_id.clone();
    match (arg(args, "client").map(str::to_string), arg(args, "action").map(str::to_string)) {
        (Some(client), Some(action)) => {
            let session = ws.store.read(cx).current.clone();
            let mut payload = json!({"client_id": client, "action": action, "origin": "palette"});
            if let Some(s) = session {
                payload["session_id"] = json!(s);
            }
            ws.store.update(cx, |s, cx| {
                s.call("client.perform", payload, cx, move |s, r, _| match r {
                    Ok(_) => s.notice = Some(format!("Ran {action}")),
                    Err(e) => s.error = Some(format!("client.perform: {}", e.message)),
                });
            });
        }
        (Some(client), None) => {
            ws.palette = Some(Palette::pick("Actions", Vec::new()).empty("Asking the client…"));
            cx.spawn(async move |this, cx| {
                let result = control.call("client.inspect", json!({"client_id": client})).await;
                this.update(cx, |ws, cx| {
                    let items: Vec<Item> = result
                        .ok()
                        .and_then(|v| v["actions"].as_array().cloned())
                        .into_iter()
                        .flatten()
                        .filter(|a| !a["args"]["required"].as_array().is_some_and(|r| !r.is_empty()))
                        .filter_map(|a| {
                            let name = a["name"].as_str()?.to_string();
                            let title = a["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(&name).to_string();
                            Some(Item::new(title, "client.other.perform", json!({"client": client, "action": name})).detail(a["category"].as_str().unwrap_or("").to_string()).keywords(name))
                        })
                        .collect();
                    if let Some(p) = ws.palette.as_mut().filter(|p| p.title == "Actions") {
                        p.items = items;
                        p.empty = "That client offers no actions without arguments".into();
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        _ => {
            ws.palette = Some(Palette::pick("Run Action on Another Client", Vec::new()).empty("Listing clients…"));
            cx.spawn(async move |this, cx| {
                let result = control.call("client.list", json!({})).await;
                this.update(cx, |ws, cx| {
                    let items: Vec<Item> = result
                        .ok()
                        .and_then(|v| v["clients"].as_array().cloned())
                        .into_iter()
                        .flatten()
                        .filter(|c| c["client_id"].as_str() != me.as_deref())
                        .filter(|c| c["connection_state"].as_str() != Some("lost"))
                        .filter_map(|c| {
                            let id = c["client_id"].as_str()?.to_string();
                            let info = &c["info"];
                            let name = [info["label"].as_str(), info["name"].as_str()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
                            let detail = format!("{} {}", info["kind"].as_str().unwrap_or(""), c["principal"]["display"].as_str().unwrap_or(""));
                            Some(Item::new(if name.is_empty() { id.clone() } else { name }, "client.other.perform", json!({"client": id})).detail(detail))
                        })
                        .collect();
                    if let Some(p) = ws.palette.as_mut().filter(|p| p.title == "Run Action on Another Client") {
                        p.items = items;
                        p.empty = "No other client is connected".into();
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
    }
    Ok(Value::Null)
}

/// Pane Methods: every method the focused block offers. Choosing one that
/// takes no required arguments runs it and shows its result.
fn block_methods(ws: &mut Workspace, args: &Value, target: &Target, cx: &mut Context<Workspace>) -> Outcome {
    let Some(view) = ws.view(cx) else { return Err(("conflict".into(), "no session is open".into())) };
    let Some(session) = view.session_id.clone() else { return Ok(Value::Null) };
    let Some(block) = arg(args, "block").map(str::to_string).or(target.block_id.clone()).or_else(|| focused_block(&view).map(str::to_string)) else {
        return Err(("conflict".into(), "no pane is focused".into()));
    };
    let Some(control) = ws.store.read(cx).control() else { return Ok(Value::Null) };
    if let Some(method) = arg(args, "method").map(str::to_string) {
        ws.palette = Some(Palette::pick(method.clone(), Vec::new()).empty("Calling…"));
        cx.spawn(async move |this, cx| {
            let result = control.block_call(&method, &session, &block, json!({})).await;
            this.update(cx, |ws, cx| {
                let lines = match result {
                    Ok(v) => result_lines(&v),
                    Err(e) => vec![format!("{}: {}", e.code, e.message)],
                };
                if let Some(p) = ws.palette.as_mut().filter(|p| p.title == method) {
                    p.items = lines.into_iter().map(|l| Item::new(l.clone(), "pane.info_copy", json!({"text": l}))).collect();
                    p.empty = "It returned nothing".into();
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        return Ok(Value::Null);
    }
    ws.palette = Some(Palette::pick("Pane Methods", Vec::new()).empty("Asking the server…"));
    cx.spawn(async move |this, cx| {
        let result = control.call("session.describe_block", json!({"session_id": session, "block_id": block})).await;
        this.update(cx, |ws, cx| {
            let v = result.unwrap_or(Value::Null);
            let creator = v["block"]["creator_name"].as_str().unwrap_or("").to_string();
            let mut items: Vec<Item> = v["api"]["methods"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(short, m)| {
                    let needs = m["input_schema"]["required"].as_array().is_some_and(|r| !r.is_empty());
                    let full = if creator.is_empty() { short.clone() } else { format!("{creator}.{short}") };
                    let item = Item::new(short.clone(), if needs { "client.palette.close" } else { "pane.methods" }, json!({"block": block, "method": full}));
                    item.detail(m["description"].as_str().unwrap_or("").to_string()).hint(if needs { "needs arguments" } else { "" })
                })
                .collect();
            items.sort_by(|a, b| a.label.cmp(&b.label));
            if let Some(p) = ws.palette.as_mut().filter(|p| p.title == "Pane Methods") {
                p.items = items;
                p.empty = "This block offers no methods".into();
            }
            cx.notify();
        })
        .ok();
    })
    .detach();
    Ok(Value::Null)
}

/// A method's result as readable lines: `key: value`, nested keys joined
/// with dots, arrays by index.
pub fn result_lines(v: &Value) -> Vec<String> {
    fn walk(prefix: &str, v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(map) if !map.is_empty() => {
                for (k, v) in map {
                    let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    walk(&key, v, out);
                }
            }
            Value::Array(items) if !items.is_empty() => {
                for (i, v) in items.iter().enumerate() {
                    walk(&format!("{prefix}[{i}]"), v, out);
                }
            }
            Value::String(s) => out.push(format!("{prefix}: {}", s.lines().next().unwrap_or(""))),
            other if prefix.is_empty() => out.push(other.to_string()),
            other => out.push(format!("{prefix}: {other}")),
        }
    }
    let mut out = Vec::new();
    walk("", v, &mut out);
    out.truncate(500);
    out
}

/// New Tab With…: one item per flavor the server can make.
fn block_types(ws: &mut Workspace, cx: &mut Context<Workspace>) -> Outcome {
    let Some(control) = ws.store.read(cx).control() else { return Ok(Value::Null) };
    ws.palette = Some(Palette::pick("New Tab With", Vec::new()).empty("Asking the server…"));
    cx.spawn(async move |this, cx| {
        let result = control.call("block_creator.list", json!({})).await;
        this.update(cx, |ws, cx| {
            let mut items = Vec::new();
            for creator in result.ok().and_then(|v| v["creators"].as_array().cloned()).into_iter().flatten() {
                for f in creator["flavors"].as_array().into_iter().flatten() {
                    let Some(flavor) = f["flavor"].as_str() else { continue };
                    let title = f["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(flavor);
                    items.push(Item::new(title, "client.tab.new_with", json!({"flavor": flavor})).detail(f["description"].as_str().unwrap_or("").to_string()).keywords(flavor));
                }
            }
            if let Some(p) = ws.palette.as_mut().filter(|p| p.title == "New Tab With") {
                p.items = items;
                p.empty = "The server makes no kinds of blocks".into();
            }
            cx.notify();
        })
        .ok();
    })
    .detach();
    Ok(Value::Null)
}

/// Clients Attached to This Session.
fn session_clients(ws: &mut Workspace, cx: &mut Context<Workspace>) -> Outcome {
    let Some(control) = ws.store.read(cx).control() else { return Ok(Value::Null) };
    let Some(session) = ws.store.read(cx).current.clone() else { return Err(("conflict".into(), "no session is open".into())) };
    ws.palette = Some(Palette::pick("Clients in This Session", Vec::new()).empty("Asking the server…"));
    cx.spawn(async move |this, cx| {
        let result = control.call("session.list_clients", json!({"session_id": session})).await;
        this.update(cx, |ws, cx| {
            let items: Vec<Item> = result
                .ok()
                .and_then(|v| v["clients"].as_array().cloned())
                .into_iter()
                .flatten()
                .filter_map(|c| {
                    let id = c["client_id"].as_str()?.to_string();
                    let info = &c["info"];
                    let name = info["name"].as_str().unwrap_or("client").to_string();
                    let detail = format!("{} {}", info["label"].as_str().unwrap_or(""), c["principal"]["display"].as_str().unwrap_or(""));
                    Some(Item::new(name, "pane.info_copy", json!({"text": id})).detail(detail.trim().to_string()))
                })
                .collect();
            if let Some(p) = ws.palette.as_mut().filter(|p| p.title == "Clients in This Session") {
                p.items = items;
                p.empty = "No client is attached".into();
            }
            cx.notify();
        })
        .ok();
    })
    .detach();
    Ok(Value::Null)
}

/// Looks a session up by any name the server understands (a label, the end
/// of an ID, ...) and shows it.
pub fn resolve_session(ws: &mut Workspace, query: String, cx: &mut Context<Workspace>) {
    let payload = json!({"kinds": ["session"], "queries": [query]});
    ws.store.update(cx, |s, cx| {
        s.call("target.resolve", payload, cx, move |s, r, cx| {
            let found = r.ok().and_then(|v| {
                let first = v["results"].as_array()?.first()?.clone();
                if first["status"].as_str() != Some("resolved") {
                    return None;
                }
                first["target"]["session_id"].as_str().map(str::to_string)
            });
            match found {
                Some(id) => s.select_session(id, cx),
                None => s.error = Some(format!("no single session matches {query}")),
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_results_into_lines() {
        let v = json!({"columns": 80, "owner": null, "desired": [{"client_id": "c", "rows": 24}], "title": "a\nb"});
        let lines = result_lines(&v);
        assert!(lines.contains(&"columns: 80".to_string()));
        assert!(lines.contains(&"owner: null".to_string()));
        assert!(lines.contains(&"desired[0].client_id: c".to_string()));
        assert!(lines.contains(&"title: a".to_string()));
        assert_eq!(result_lines(&json!({})), vec!["{}".to_string()]);
    }
}
