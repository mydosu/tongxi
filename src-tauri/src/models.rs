use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Serialize, Debug)]
pub struct ModelOption {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
}

pub fn codex_models(value: &Value) -> Vec<ModelOption> {
    value["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some(ModelOption {
                id: entry["model"].as_str()?.into(),
                name: entry["displayName"]
                    .as_str()
                    .unwrap_or(entry["model"].as_str()?)
                    .into(),
                provider_id: None,
                provider_name: None,
                efforts: entry["supportedReasoningEfforts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|effort| effort["reasoningEffort"].as_str().map(String::from))
                    .collect(),
                default_effort: entry["defaultReasoningEffort"].as_str().map(String::from),
            })
        })
        .collect()
}

pub fn validate_selection(
    models: &[ModelOption],
    model: Option<&str>,
    effort: Option<&str>,
    default_model: Option<&str>,
) -> Result<(), String> {
    if model.is_none() && effort.is_none() {
        return Ok(());
    }
    let selected = model
        .or(default_model)
        .ok_or("请先连接成员以读取模型能力")?;
    let capability = models
        .iter()
        .find(|item| item.id == selected)
        .ok_or("当前 harness 模型目录中没有这个模型，请重新选择")?;
    if effort.is_some_and(|level| !capability.efforts.iter().any(|allowed| allowed == level)) {
        return Err("所选模型不支持这个思考强度，请重新选择".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capability_validation_rejects_wrong_model_or_effort() {
        let catalog = codex_models(
            &serde_json::json!({"data":[{"model":"example","displayName":"Example","supportedReasoningEfforts":[{"reasoningEffort":"low"}]}]}),
        );
        assert!(validate_selection(&catalog, Some("example"), Some("low"), None).is_ok());
        assert!(validate_selection(&catalog, Some("other"), None, None).is_err());
        assert!(validate_selection(&catalog, Some("example"), Some("ultra"), None).is_err());
        assert!(validate_selection(&catalog, None, Some("low"), Some("example")).is_ok());
    }
}
