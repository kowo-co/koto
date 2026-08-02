//! AT-SPI2 structured accessibility observation.

use atspi_connection::AccessibilityConnection;
use atspi_proxies::accessible::{AccessibleProxy, ObjectRefExt};
use futures_executor::block_on;
use koto_core::{CoreError, Observation};

/// Collects a bounded textual projection of the accessibility tree. AT-SPI
/// coverage is application-dependent, so an absent or empty tree simply lets
/// the observation ladder continue to the next rung.
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
        let root = if let Some(pid) = pid {
            find_application(&root, connection.connection(), pid)
                .await
                .unwrap_or(root)
        } else {
            root
        };
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

async fn find_application<'a>(
    root: &AccessibleProxy<'a>,
    connection: &'a zbus::Connection,
    pid: i32,
) -> Option<AccessibleProxy<'a>> {
    let children = root.get_children().await.ok()?;
    for child in children {
        let proxy = child.into_accessible_proxy(connection).await.ok()?;
        let application = atspi_proxies::application::ApplicationProxy::builder(connection)
            .destination(proxy.inner().destination().clone())
            .ok()?
            .build()
            .await
            .ok()?;
        if application.id().await.ok()? == pid {
            return Some(proxy);
        }
    }
    None
}
async fn collect(
    proxy: &AccessibleProxy<'_>,
    connection: &zbus::Connection,
    depth: u8,
    lines: &mut Vec<String>,
) {
    if depth > 8 || lines.len() >= 512 {
        return;
    }
    if let Ok(name) = proxy.name().await {
        if !name.trim().is_empty() {
            lines.push(name);
        }
    }
    let Ok(children) = proxy.get_children().await else {
        return;
    };
    for child in children {
        if lines.len() >= 512 {
            return;
        }
        if let Ok(proxy) = child.as_accessible_proxy(connection).await {
            Box::pin(collect(&proxy, connection, depth + 1, lines)).await;
        }
    }
}
