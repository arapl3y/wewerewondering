use crate::get_dynamo_timestamp;

use super::{Backend, Local};
use aws_sdk_dynamodb::{
    error::UpdateItemError, model::AttributeValue, output::UpdateItemOutput, types::SdkError,
};
use axum::extract::{Path, State};
use http::StatusCode;
use serde::Deserialize;
use std::{collections::HashMap, time::SystemTime};
use uuid::Uuid;

#[allow(unused_imports)]
use tracing::{debug, error, info, trace, warn};

#[derive(Deserialize, Debug, Copy, Clone)]
#[serde(rename_all = "lowercase")]
pub(super) enum Property {
    Hidden,
    Answered,
}

impl Backend {
    pub(super) async fn toggle(
        &self,
        qid: &Uuid,
        property: Property,
        set: bool,
    ) -> Result<UpdateItemOutput, SdkError<UpdateItemError>> {
        match self {
            Self::Dynamo(dynamo) => {
                let q = dynamo
                    .update_item()
                    .table_name("questions")
                    .key("id", AttributeValue::S(qid.to_string()));

                let q = match property {
                    Property::Hidden => q
                        .update_expression("SET #field = :set")
                        .expression_attribute_names("#field", "hidden")
                        .expression_attribute_values(":set", AttributeValue::Bool(set)),
                    Property::Answered => {
                        if set {
                            q.update_expression("SET #field = :set")
                                .expression_attribute_names("#field", "answered")
                                .expression_attribute_values(
                                    ":set",
                                    get_dynamo_timestamp(SystemTime::now()),
                                )
                        } else {
                            q.update_expression("REMOVE #field")
                                .expression_attribute_names("#field", "answered")
                        }
                    }
                };
                q.send().await
            }
            Self::Local(local) => {
                let mut local = local.lock().unwrap();
                let Local { questions, .. } = &mut *local;

                fn invert(q: &mut HashMap<&'static str, AttributeValue>, key: &'static str) {
                    if let AttributeValue::Bool(b) = q[key] {
                        q.insert(key, AttributeValue::Bool(!b));
                    } else {
                        unreachable!("all properties are bools");
                    }
                }

                let q = questions
                    .get_mut(qid)
                    .expect("toggle property on unknown question ");
                match property {
                    Property::Hidden => invert(q, "hidden"),
                    Property::Answered => {
                        if q.contains_key("answered") {
                            q.remove("answered");
                        } else {
                            q.insert("answered", get_dynamo_timestamp(SystemTime::now()));
                        }
                    }
                }

                Ok(UpdateItemOutput::builder().build())
            }
        }
    }
}

pub(super) async fn toggle(
    Path((eid, secret, qid, property)): Path<(Uuid, String, Uuid, Property)>,
    State(dynamo): State<Backend>,
    body: String,
) -> Result<(), StatusCode> {
    super::check_secret(&dynamo, &eid, &secret).await?;

    let set = match &*body {
        "on" => true,
        "off" => false,
        _ => {
            error!(%qid, body, "invalid toggle value");
            return Err(http::StatusCode::BAD_REQUEST);
        }
    };

    match dynamo.toggle(&qid, property, set).await {
        Ok(_) => {
            debug!(%eid, %qid, p = ?property, "toggled question property");
            Ok(())
        }
        Err(e) => {
            error!(%qid, error = %e, "dynamodb request to vote for question failed");
            Err(http::StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;

    async fn inner(backend: Backend) {
        let e = crate::new::new(State(backend.clone())).await.unwrap();
        let eid = Uuid::parse_str(e["id"].as_str().unwrap()).unwrap();
        let secret = e["secret"].as_str().unwrap();
        let q = crate::ask::ask(
            Path(eid.clone()),
            State(backend.clone()),
            Json(crate::ask::Question {
                body: "hello world".into(),
                asker: None,
            }),
        )
        .await
        .unwrap();
        let qid = q["id"].as_str().unwrap();
        let qid_u = Uuid::parse_str(qid).unwrap();

        let check =
            |qids: serde_json::Value,
             expect: Option<(bool, Box<dyn Fn(&serde_json::Value) -> bool>, u64)>| {
                let qids = qids.as_array().unwrap();
                let q = qids.iter().find(|q| dbg!(&q["qid"]) == dbg!(qid));
                if let Some((hidden, check_answered, votes)) = expect {
                    assert_ne!(
                        q, None,
                        "newly created question {qid} was not listed in {qids:?}"
                    );
                    let q = q.unwrap();
                    assert_eq!(q["votes"].as_u64().unwrap(), votes);
                    assert!(check_answered(&q["answered"]));
                    assert_eq!(q["hidden"].as_bool().unwrap(), hidden);
                    assert_eq!(qids.len(), 1, "extra questions in response: {qids:?}");
                } else {
                    assert_eq!(
                        q, None,
                        "newly created question {qid} was not listed in {qids:?}"
                    );
                }
            };

        // only admin should see hidden
        super::toggle(
            Path((
                eid.clone(),
                secret.to_string(),
                qid_u.clone(),
                Property::Hidden,
            )),
            State(backend.clone()),
            String::from("on"),
        )
        .await
        .unwrap();
        check(
            crate::list::list_all(
                Path((eid.clone(), secret.to_string())),
                State(backend.clone()),
            )
            .await
            .1
            .unwrap()
            .0,
            Some((true, Box::new(|v| v.is_null()), 1)),
        );
        check(
            crate::list::list(Path(eid.clone()), State(backend.clone()))
                .await
                .1
                .unwrap()
                .0,
            None,
        );

        // should toggle back
        super::toggle(
            Path((
                eid.clone(),
                secret.to_string(),
                qid_u.clone(),
                Property::Hidden,
            )),
            State(backend.clone()),
            String::from("off"),
        )
        .await
        .unwrap();
        // and should now show up as answered
        super::toggle(
            Path((
                eid.clone(),
                secret.to_string(),
                qid_u.clone(),
                Property::Answered,
            )),
            State(backend.clone()),
            String::from("on"),
        )
        .await
        .unwrap();
        check(
            crate::list::list_all(
                Path((eid.clone(), secret.to_string())),
                State(backend.clone()),
            )
            .await
            .1
            .unwrap()
            .0,
            Some((false, Box::new(|v| v.is_u64()), 1)),
        );
        check(
            crate::list::list(Path(eid.clone()), State(backend.clone()))
                .await
                .1
                .unwrap()
                .0,
            Some((false, Box::new(|v| v.is_u64()), 1)),
        );

        backend.delete(&eid).await;
    }

    #[tokio::test]
    async fn local() {
        inner(Backend::local().await).await;
    }

    #[tokio::test]
    #[ignore]
    async fn dynamodb() {
        inner(Backend::dynamo().await).await;
    }
}
