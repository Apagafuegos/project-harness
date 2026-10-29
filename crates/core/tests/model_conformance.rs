//! Public API consumer: no access to private runtime helpers or fixture internals.
#![cfg(feature = "conformance")]

use std::{collections::BTreeMap, sync::Arc};

use harness_core::{
    model::{
        InferenceOptions, ModelInput, ModelRequest,
        conformance::{
            Case, CaseKind, Expected, Fixture, FixtureFactory, Observation, SubmittedRequest,
            SubmittedTool, run_case,
        },
        scripted::{Script, ScriptedProvider, Step},
    },
    session::{FinishReason, ModelRef, ResponseId},
};

struct RecordedFixtures;
impl FixtureFactory for RecordedFixtures {
    fn build(&self, fixture: &str) -> Fixture {
        assert_eq!(fixture, "recorded_text");
        let provider = Arc::new(ScriptedProvider::new([Script::new([
            Step::Event(harness_core::model::ProviderEvent::Started),
            Step::text(0, "external consumer"),
            Step::completed(FinishReason::Stop),
        ])]));
        let transport = provider.clone();
        Fixture {
            provider,
            observe: Arc::new(move || {
                let records = transport.streams();
                let record = &records[0];
                Observation {
                    submitted: Some(SubmittedRequest {
                        tools: record
                            .request
                            .input
                            .tools
                            .iter()
                            .map(SubmittedTool::from)
                            .collect(),
                        options: record.request.input.options.clone(),
                    }),
                    released: record.dropped,
                }
            }),
        }
    }
}

#[tokio::test]
async fn public_factory_and_manifest_are_usable_outside_the_crate() {
    let case = Case {
        kind: CaseKind::Text,
        fixture: "recorded_text".into(),
        request: ModelRequest {
            response_id: ResponseId::try_from("019a0000-0000-7000-8000-000000000001".to_owned())
                .unwrap(),
            artifacts: Default::default(),
            input: ModelInput {
                model: ModelRef {
                    provider_instance: "fixture".into(),
                    model: "model".into(),
                },
                items: Vec::new(),
                tools: Vec::new(),
                options: InferenceOptions {
                    max_output_tokens: 100,
                    reasoning_effort: None,
                    provider_options: Default::default(),
                },
                continuation: None,
            },
        },
        expected: Expected {
            text: BTreeMap::from([(0, "external consumer".into())]),
            ..Expected::default()
        },
        unsupported_option: None,
    };
    run_case(&RecordedFixtures, &case).await.unwrap();
}
