//! Model-facing content, independent of the complete MCP responses stored in entities.
use frances_mcp::{
    CallToolResult, ContentBlock, GetPromptResult, ReadResourceResult, Resource, ResourceContents,
    ResourceTemplate,
};

pub(super) fn tool(result: &CallToolResult) -> String {
    let mut parts: Vec<_> = result.content.iter().map(content).collect();
    if let Some(structured) = &result.structured_content {
        // Servers commonly supply the same structured result as a text block too.
        let already_present = result.content.iter().any(|block| {
            matches!(block, ContentBlock::Text(text) if serde_json::from_str::<serde_json::Value>(&text.text).is_ok_and(|value| &value == structured))
        });
        if !already_present {
            parts.push(format!(
                "Structured result:\n{}",
                serde_json::to_string_pretty(structured).expect("JSON value serializes")
            ));
        }
    }
    if parts.is_empty() {
        "No content returned.".into()
    } else {
        parts.join("\n\n")
    }
}

pub(super) fn resources(result: &ReadResourceResult) -> String {
    if result.contents.is_empty() {
        return "No resource content returned.".into();
    }
    result
        .contents
        .iter()
        .map(resource_content)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn resource_content(resource: &ResourceContents) -> String {
    match resource {
        ResourceContents::TextResourceContents { uri, text, .. } => {
            format!("Resource: {uri}\n\n{text}")
        }
        ResourceContents::BlobResourceContents { uri, .. } => format!(
            "Resource: {uri}\n\n[Binary resource retained in the transcript; unavailable through the model's text interface.]"
        ),
        _ => "[Unsupported resource content retained in the transcript.]".into(),
    }
}

fn content(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::Image(_) => {
            "[Image retained in the transcript; unavailable through the model's text interface.]"
                .into()
        }
        ContentBlock::Audio(_) => {
            "[Audio retained in the transcript; unavailable through the model's text interface.]"
                .into()
        }
        ContentBlock::Resource(resource) => resource_content(&resource.resource),
        ContentBlock::ResourceLink(resource) => resource_description(resource),
        _ => "[Unsupported content retained in the transcript.]".into(),
    }
}

fn resource_description(resource: &Resource) -> String {
    let mut text = format!(
        "{} — {}",
        resource.title.as_deref().unwrap_or(&resource.name),
        resource.uri
    );
    if let Some(mime) = &resource.mime_type {
        text.push_str(&format!(" ({mime})"));
    }
    if let Some(description) = &resource.description {
        text.push_str(&format!("\n{description}"));
    }
    text
}

pub(super) fn catalog(resources: &[Resource], templates: &[ResourceTemplate]) -> String {
    if resources.is_empty() && templates.is_empty() {
        return "No resources or resource templates available.".into();
    }
    let mut parts = Vec::new();
    if !resources.is_empty() {
        parts.push(format!(
            "Resources:\n{}",
            resources
                .iter()
                .map(|resource| format!("- {}", resource_description(resource)))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !templates.is_empty() {
        let entries: Vec<_> = templates
            .iter()
            .map(|template| {
                let mut text = format!(
                    "- {} — {}",
                    template.title.as_deref().unwrap_or(&template.name),
                    template.uri_template
                );
                if let Some(mime) = &template.mime_type {
                    text.push_str(&format!(" ({mime})"));
                }
                if let Some(description) = &template.description {
                    text.push_str(&format!("\n{description}"));
                }
                text
            })
            .collect();
        parts.push(format!(
            "Resource templates (fill in URI parameters before reading):\n{}",
            entries.join("\n")
        ));
    }
    parts.join("\n\n")
}

pub(super) fn prompt(result: &GetPromptResult) -> String {
    let mut parts = Vec::new();
    if let Some(description) = &result.description {
        parts.push(description.clone());
    }
    for message in &result.messages {
        parts.push(format!(
            "{:?}:\n{}",
            message.role,
            content(&message.content)
        ));
    }
    if parts.is_empty() {
        "No prompt content returned.".into()
    } else {
        parts.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_is_verbatim_without_protocol_fields() {
        let result = serde_json::from_value(json!({"content":[{"type":"text","text":"First line\nSecond line","_meta":{"secret":"hidden"}}],"_meta":{"secret":"hidden"},"isError":false})).unwrap();
        assert_eq!(tool(&result), "First line\nSecond line");
    }

    #[test]
    fn resource_reads_and_embedded_resources_show_uri_and_text() {
        let resource = json!({"uri":"file:///guide.md","text":"# Guide\nUse this directly.","_meta":{"secret":"hidden"}});
        let result = serde_json::from_value(json!({"contents":[resource.clone()]})).unwrap();
        let expected = "Resource: file:///guide.md\n\n# Guide\nUse this directly.";
        assert_eq!(resources(&result), expected);
        let result =
            serde_json::from_value(json!({"content":[{"type":"resource","resource":resource}]}))
                .unwrap();
        assert_eq!(tool(&result), expected);
    }

    #[test]
    fn catalog_preserves_discovery_details_without_wire_metadata() {
        let resource = serde_json::from_value(json!({"name":"guide","title":"Guide","uri":"docs://guide","description":"Getting started","mimeType":"text/markdown","_meta":{"secret":"hidden"}})).unwrap();
        let template = serde_json::from_value(
            json!({"name":"Page","uriTemplate":"docs://{page}","description":"Choose a page"}),
        )
        .unwrap();
        assert_eq!(
            catalog(&[resource], &[template]),
            "Resources:\n- Guide — docs://guide (text/markdown)\nGetting started\n\nResource templates (fill in URI parameters before reading):\n- Page — docs://{page}\nChoose a page"
        );
    }

    #[test]
    fn structured_tool_data_is_preserved_without_duplicate_json() {
        let data = json!({"_meta":"user data","blob":"user text","count":2});
        let result =
            serde_json::from_value(json!({"content":[],"structuredContent":data})).unwrap();
        assert_eq!(
            tool(&result),
            format!(
                "Structured result:\n{}",
                serde_json::to_string_pretty(&data).unwrap()
            )
        );
        let result = serde_json::from_value(
            json!({"content":[{"type":"text","text":data.to_string()}],"structuredContent":data}),
        )
        .unwrap();
        assert_eq!(tool(&result), data.to_string());
    }

    #[test]
    fn prompt_roles_and_binary_notices_are_readable() {
        let result = serde_json::from_value(json!({"description":"Review this","messages":[{"role":"user","content":{"type":"text","text":"Read carefully"}},{"role":"assistant","content":{"type":"image","mimeType":"image/png","data":"secret-base64"}}]})).unwrap();
        let text = prompt(&result);
        assert!(text.starts_with("Review this\n\nUser:\nRead carefully\n\nAssistant:\n[Image"));
        assert!(!text.contains("secret-base64"));
        let result = serde_json::from_value(
            json!({"contents":[{"uri":"file:///image","blob":"secret-base64"}]}),
        )
        .unwrap();
        let text = resources(&result);
        assert!(text.contains("Binary resource"));
        assert!(!text.contains("secret-base64"));
    }
}
