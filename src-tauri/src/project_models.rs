//! Select a project task model and reasoning effort from the native model catalog.
use crate::models::ModelOption;
use crate::project_store::ExecutionChoice;

type Result<T> = std::result::Result<T, String>;

/// Resolve a project attempt's model and effort without making a model request.
/// Manual selections are validated against the connected member's real catalog.
pub fn select(
    models: &[ModelOption],
    default_model: Option<&str>,
    manual_model: Option<&str>,
    manual_effort: Option<&str>,
    stage: &str,
    proposal: Option<&ExecutionChoice>,
) -> Result<(String, String)> {
    if models.is_empty() {
        return Err("当前成员没有可用的原生模型目录".into());
    }

    let manual_model = nonempty(manual_model);
    let manual_effort = nonempty(manual_effort);
    let proposal = if manual_model.is_some() {
        None
    } else {
        proposal
    };

    let selected_id = if let Some(model) = manual_model {
        model
    } else if stage == "implement" {
        if let Some(choice) = proposal {
            if choice.model.trim().is_empty() {
                return Err("Codex 方案没有给出有效模型".into());
            }
            if is_astra(&choice.model) {
                return Err("自动选型不能调度 gpt-6-astra".into());
            }
            &choice.model
        } else {
            preferred_or_default(models, "gpt-6-luna", default_model)?
        }
    } else if matches!(stage, "plan" | "repair") {
        preferred_or_default(models, "gpt-6.1-sol", default_model)?
    } else {
        default_model.ok_or("本机没有默认模型，无法自动选型")?
    };

    let selected = models
        .iter()
        .find(|item| item.id == selected_id)
        .ok_or("当前模型目录不包含所选模型")?;

    if manual_model.is_none() && is_astra(&selected.id) {
        return Err("自动选型不能调度 gpt-6-astra".into());
    }

    let requested_effort = if let Some(effort) = manual_effort {
        Some(effort)
    } else if stage == "implement" {
        proposal.and_then(|choice| choice.reasoning_effort.as_deref())
    } else {
        None
    };

    let effort = if let Some(effort) = requested_effort {
        if !selected.efforts.iter().any(|allowed| allowed == effort) {
            return Err("所选模型不支持该思考强度".into());
        }
        if manual_effort.is_none() && effort == "ultra" {
            return Err("自动方案不能指定 ultra 思考强度".into());
        }
        effort.to_owned()
    } else {
        let preferred = match stage {
            "plan" => Some("medium"),
            "repair" => Some("high"),
            "implement" => Some("medium"),
            _ => None,
        };
        preferred
            .filter(|effort| selected.efforts.iter().any(|allowed| allowed == effort))
            .or_else(|| {
                selected
                    .default_effort
                    .as_deref()
                    .filter(|effort| selected.efforts.iter().any(|allowed| allowed == effort))
                    .filter(|effort| *effort != "ultra")
            })
            .or_else(|| {
                selected
                    .efforts
                    .iter()
                    .find(|effort| effort.as_str() != "ultra")
                    .map(String::as_str)
            })
            .map(str::to_owned)
            .ok_or("当前模型没有可自动使用的思考强度")?
    };

    Ok((selected.id.clone(), effort))
}

fn preferred_or_default<'a>(
    models: &'a [ModelOption],
    preferred: &'a str,
    default_model: Option<&'a str>,
) -> Result<&'a str> {
    if models.iter().any(|item| item.id == preferred) {
        return Ok(preferred);
    }
    default_model.ok_or_else(|| "首选模型不在目录中，且没有本机默认模型".into())
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn is_astra(model: &str) -> bool {
    model.to_ascii_lowercase().contains("gpt-6-astra")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(id: &str, efforts: &[&str], default_effort: Option<&str>) -> ModelOption {
        ModelOption {
            id: id.into(),
            name: id.into(),
            efforts: efforts.iter().map(|effort| (*effort).into()).collect(),
            default_effort: default_effort.map(str::to_owned),
        }
    }

    fn catalog() -> Vec<ModelOption> {
        vec![
            option("gpt-6-luna", &["low", "medium", "high"], Some("low")),
            option(
                "gpt-6.1-sol",
                &["low", "medium", "high", "xhigh"],
                Some("medium"),
            ),
            option("gpt-6-astra", &["low", "ultra"], Some("low")),
            option("local-default", &["low", "high"], Some("low")),
        ]
    }

    fn choice(model: &str, effort: Option<&str>) -> ExecutionChoice {
        ExecutionChoice {
            model: model.into(),
            reasoning_effort: effort.map(str::to_owned),
            rationale: "test rationale".into(),
        }
    }

    #[test]
    fn manual_selection_takes_priority_over_proposal() {
        assert_eq!(
            select(
                &catalog(),
                Some("local-default"),
                Some("gpt-6.1-sol"),
                Some("high"),
                "implement",
                Some(&choice("missing-model", Some("ultra")))
            )
            .unwrap(),
            ("gpt-6.1-sol".into(), "high".into())
        );
    }

    #[test]
    fn implementation_uses_the_proposed_choice() {
        assert_eq!(
            select(
                &catalog(),
                Some("local-default"),
                None,
                None,
                "implement",
                Some(&choice("gpt-6.1-sol", Some("xhigh")))
            )
            .unwrap(),
            ("gpt-6.1-sol".into(), "xhigh".into())
        );
    }

    #[test]
    fn unknown_proposal_model_fails_closed() {
        assert!(select(
            &catalog(),
            Some("local-default"),
            None,
            None,
            "implement",
            Some(&choice("missing-model", None))
        )
        .is_err());
    }

    #[test]
    fn unsupported_proposal_effort_fails_closed() {
        assert!(select(
            &catalog(),
            Some("local-default"),
            None,
            None,
            "implement",
            Some(&choice("gpt-6-luna", Some("xhigh")))
        )
        .is_err());
    }

    #[test]
    fn unsupported_manual_effort_fails_closed() {
        assert!(select(
            &catalog(),
            Some("local-default"),
            Some("local-default"),
            Some("medium"),
            "implement",
            None
        )
        .is_err());
    }

    #[test]
    fn plan_and_repair_prefer_sol_with_stage_efforts() {
        let models = catalog();
        assert_eq!(
            select(&models, Some("local-default"), None, None, "plan", None).unwrap(),
            ("gpt-6.1-sol".into(), "medium".into())
        );
        assert_eq!(
            select(&models, Some("local-default"), None, None, "repair", None).unwrap(),
            ("gpt-6.1-sol".into(), "high".into())
        );
    }

    #[test]
    fn implementation_without_proposal_prefers_luna() {
        assert_eq!(
            select(
                &catalog(),
                Some("local-default"),
                None,
                None,
                "implement",
                None
            )
            .unwrap(),
            ("gpt-6-luna".into(), "medium".into())
        );
    }

    #[test]
    fn missing_tier_preference_falls_back_to_default_catalog_model() {
        let models = vec![option("local-default", &["low", "high"], Some("low"))];
        assert_eq!(
            select(&models, Some("local-default"), None, None, "plan", None).unwrap(),
            ("local-default".into(), "low".into())
        );
        assert!(select(&models, Some("not-in-catalog"), None, None, "plan", None).is_err());
    }

    #[test]
    fn unsupported_automatic_effort_uses_model_default_then_allowed_effort() {
        let with_default = vec![option("local-default", &["low", "high"], Some("high"))];
        assert_eq!(
            select(
                &with_default,
                Some("local-default"),
                None,
                None,
                "implement",
                None
            )
            .unwrap(),
            ("local-default".into(), "high".into())
        );
        let first_allowed = vec![option("local-default", &["low", "high"], None)];
        assert_eq!(
            select(
                &first_allowed,
                Some("local-default"),
                None,
                None,
                "implement",
                None
            )
            .unwrap(),
            ("local-default".into(), "low".into())
        );
    }

    #[test]
    fn empty_catalog_fails_even_with_explicit_model() {
        assert!(select(
            &[],
            Some("gpt-6-luna"),
            Some("gpt-6-luna"),
            None,
            "implement",
            None
        )
        .is_err());
    }

    #[test]
    fn automatic_selection_never_uses_astra_or_ultra_but_manual_can() {
        assert!(select(&catalog(), Some("gpt-6-astra"), None, None, "review", None).is_err());
        assert!(select(
            &catalog(),
            Some("local-default"),
            None,
            None,
            "implement",
            Some(&choice("gpt-6-luna", Some("ultra")))
        )
        .is_err());
        assert_eq!(
            select(
                &catalog(),
                None,
                Some("gpt-6-astra"),
                Some("ultra"),
                "implement",
                None
            )
            .unwrap(),
            ("gpt-6-astra".into(), "ultra".into())
        );
    }
}
