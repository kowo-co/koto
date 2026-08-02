//! AT-SPI2 structured accessibility observation.

use atspi_connection::AccessibilityConnection;
use atspi_proxies::accessible::{AccessibleProxy, ObjectRefExt};
use atspi_proxies::proxy_ext::ProxyExt;
use futures_executor::block_on;
use koto_core::{CoreError, Observation};

/// Collects a bounded textual projection of the accessibility tree. AT-SPI
/// coverage is application-dependent, so an absent or empty tree simply lets
/// the observation ladder continue to the next rung.
///
/// Each line carries the element's role, name, and interesting states, indented
/// by depth. Names alone are not actionable: an agent deciding whether to flip a
/// switch has to know that the element *is* a switch and whether it is already
/// checked, and without that it has no choice but to fall back to pixels.
pub fn observe_focused(pid: Option<i32>) -> Result<Option<Observation>, CoreError> {
    block_on(async {
        let connection = match AccessibilityConnection::new().await {
            Ok(connection) => connection,
            Err(_) => return Ok(None),
        };
        let root = match connection.root_accessible_on_registry().await {
            Ok(root) => root,
            Err(_) => return Ok(None),
        };
        let scoped = match pid {
            Some(pid) => find_application(&root, connection.connection(), pid).await,
            None => None,
        };
        // Scope to the focused application when we can identify it. Falling back
        // to the registry root dumps every running application, burying the one
        // the caller asked about.
        let root = scoped.unwrap_or(root);
        let mut lines = Vec::new();
        collect(&root, connection.connection(), 0, &mut lines).await;
        if lines.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Observation {
                source: "atspi".into(),
                fidelity: "structured".into(),
                text: Some(lines.join("\n")),
                image: None,
            }))
        }
    })
}

/// Finds the application owned by `pid`.
///
/// The AT-SPI `Application.Id` property is an application-assigned identifier,
/// not a process id, so it cannot be compared against one. The owning process is
/// resolved through the bus instead, by asking who owns the object's connection.
async fn find_application<'a>(
    root: &AccessibleProxy<'a>,
    connection: &'a zbus::Connection,
    pid: i32,
) -> Option<AccessibleProxy<'a>> {
    let bus = zbus::fdo::DBusProxy::new(connection).await.ok()?;
    let children = root.get_children().await.ok()?;
    for child in children {
        let Ok(proxy) = child.into_accessible_proxy(connection).await else {
            continue;
        };
        let destination = proxy.inner().destination().to_owned();
        let Ok(owner) = bus.get_connection_unix_process_id(destination.into()).await else {
            continue;
        };
        if owner as i32 == pid {
            return Some(proxy);
        }
    }
    None
}

/// States worth reporting: the ones that change what an agent should do next.
async fn interesting_states(proxy: &AccessibleProxy<'_>) -> Vec<&'static str> {
    use atspi_common::State;
    let Ok(states) = proxy.get_state().await else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for (state, label) in [
        (State::Checked, "checked"),
        (State::Expanded, "expanded"),
        (State::Selected, "selected"),
        (State::Focused, "focused"),
        (State::Editable, "editable"),
    ] {
        if states.contains(state) {
            found.push(label);
        }
    }
    found
}

/// Containers do not advertise Enabled/Sensitive, so a missing flag there means
/// "not applicable" rather than "greyed out"; only say disabled for controls.
fn is_control(role: &str) -> bool {
    matches!(
        role,
        "switch"
            | "check box"
            | "radio button"
            | "push button"
            | "button"
            | "toggle button"
            | "menu item"
            | "text"
            | "entry"
            | "combo box"
            | "slider"
            | "spin button"
            | "link"
            | "tab"
    )
}

async fn is_enabled(proxy: &AccessibleProxy<'_>) -> bool {
    use atspi_common::State;
    match proxy.get_state().await {
        Ok(states) => states.contains(State::Enabled) || states.contains(State::Sensitive),
        Err(_) => true,
    }
}

async fn collect(
    proxy: &AccessibleProxy<'_>,
    connection: &zbus::Connection,
    depth: u8,
    lines: &mut Vec<String>,
) {
    if depth > 8 || lines.len() >= 256 {
        return;
    }
    let name = proxy.name().await.unwrap_or_default();
    let role = proxy
        .get_role_name()
        .await
        .unwrap_or_else(|_| String::from("unknown"));
    // Filler nodes carry neither a name nor an actionable role; printing them
    // just pads the projection out of its budget.
    let structural = matches!(role.as_str(), "filler" | "panel" | "section" | "unknown");
    if !name.trim().is_empty() || !structural {
        let mut states = interesting_states(proxy).await;
        if is_control(&role) && !is_enabled(proxy).await {
            states.push("disabled");
        }
        let mut line = format!("{:indent$}{role}", "", indent = depth as usize * 2);
        if !name.trim().is_empty() {
            line.push_str(&format!(" \"{}\"", name.trim()));
        }
        if !states.is_empty() {
            line.push_str(&format!(" [{}]", states.join(",")));
        }
        lines.push(line);
    }
    let Ok(children) = proxy.get_children().await else {
        return;
    };
    for child in children {
        if lines.len() >= 256 {
            return;
        }
        if let Ok(proxy) = child.as_accessible_proxy(connection).await {
            Box::pin(collect(&proxy, connection, depth + 1, lines)).await;
        }
    }
}
