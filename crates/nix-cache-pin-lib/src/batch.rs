//! Select and classify batch searches without performing searches or writes.

use crate::{config::PinConfig, error::Error, runner::FindResult};

/// A requested pin name that was not present in the supplied configurations.
#[derive(Debug, thiserror::Error)]
#[error("unknown pin '{name}' (configured: {})", configured.join(", "))]
pub struct UnknownPin {
    pub name: String,
    pub configured: Vec<String>,
}

/// Keep the requested pin and all pins sharing its input, in original order.
/// Without a pin name, keep all configurations.
pub fn select_configs(
    mut configs: Vec<PinConfig>,
    pin: Option<&str>,
) -> Result<Vec<PinConfig>, UnknownPin> {
    if let Some(name) = pin {
        let Some(input) = configs
            .iter()
            .find(|cfg| cfg.name == name)
            .map(|cfg| cfg.input_name.clone())
        else {
            return Err(UnknownPin {
                name: name.into(),
                configured: configs.into_iter().map(|cfg| cfg.name).collect(),
            });
        };
        configs.retain(|cfg| cfg.input_name == input);
    }
    Ok(configs)
}

#[derive(Debug)]
pub struct SearchFailure {
    pub pin: String,
    pub reason: SearchFailureReason,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchFailureReason {
    #[error("no revision found with all packages cached")]
    NoRevision,
    #[error(transparent)]
    Search(Error),
}

/// Return `(config, revision)` apply candidates only if every search succeeded,
/// or all failures otherwise. Preserve input order in either case.
///
/// Inspect logs and probes on the original results before calling this
/// function. It consumes those results but does not apply any changes.
pub fn classify_search_results(
    results: Vec<FindResult>,
) -> Result<Vec<(PinConfig, String)>, Vec<SearchFailure>> {
    let mut candidates = Vec::with_capacity(results.len());
    let mut failures = Vec::new();
    for FindResult {
        config, target_rev, ..
    } in results
    {
        let reason = match target_rev {
            Ok(Some(rev)) => {
                candidates.push((config, rev));
                continue;
            }
            Ok(None) => SearchFailureReason::NoRevision,
            Err(error) => SearchFailureReason::Search(error),
        };
        failures.push(SearchFailure {
            pin: config.name,
            reason,
        });
    }
    if failures.is_empty() {
        Ok(candidates)
    } else {
        Err(failures)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Output;

    fn pin(name: &str, input: &str) -> PinConfig {
        crate::config::test_pin(name, input)
    }

    #[test]
    fn selects_every_pin_for_the_named_pins_input_in_original_order() {
        let pins = vec![
            pin("desktop", "unstable"),
            pin("gpu", "rocm"),
            pin("server", "unstable"),
        ];
        assert_eq!(select_configs(pins.clone(), None).unwrap().len(), 3);
        let error = select_configs(pins.clone(), Some("missing")).unwrap_err();
        assert!(error.to_string().contains("unknown pin 'missing'"));
        assert!(error.to_string().contains("desktop, gpu, server"));
        let selected = select_configs(pins, Some("server")).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|pin| pin.name.as_str())
                .collect::<Vec<_>>(),
            ["desktop", "server"]
        );
    }

    #[test]
    fn classifies_searches_without_returning_partial_candidates() {
        let search = |name: &str, target_rev| FindResult {
            config: pin(name, name),
            output: Output::buffered(name),
            target_rev,
        };
        assert!(classify_search_results(vec![]).unwrap().is_empty());
        let ready = classify_search_results(vec![
            search("first", Ok(Some("abc".into()))),
            search("second", Ok(Some("def".into()))),
        ])
        .unwrap();
        assert_eq!(ready[0].0.name, "first");
        assert_eq!(ready[0].1, "abc");
        assert_eq!(ready[1].0.name, "second");
        assert_eq!(ready[1].1, "def");

        let failures = classify_search_results(vec![
            search("first", Ok(Some("abc".into()))),
            search("miss", Ok(None)),
            search("error", Err(Error::FailFast { rev: "bad".into() })),
        ])
        .unwrap_err();
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].pin, "miss");
        assert!(matches!(
            failures[0].reason,
            SearchFailureReason::NoRevision
        ));
        assert_eq!(failures[1].pin, "error");
        assert!(matches!(
            &failures[1].reason,
            SearchFailureReason::Search(Error::FailFast { rev }) if rev == "bad"
        ));
    }
}
