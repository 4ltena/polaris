//! Strict tagged answers and duplicate-key refusal shared by typed protocols.
use super::{Answer, DecisionError, Question};
use serde::{
    Deserialize, Deserializer,
    de::{Error, MapAccess, Visitor},
};
use std::{collections::BTreeMap, fmt};
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireAnswer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        #[serde(deserialize_with = "unique_map")]
        probabilities: BTreeMap<String, f64>,
        confidence: Option<f64>,
    },
    Score {
        score: f64,
        #[serde(deserialize_with = "unique_map")]
        probabilities: BTreeMap<String, f64>,
        #[serde(deserialize_with = "unique_map")]
        legend: BTreeMap<String, String>,
        confidence: Option<f64>,
    },
}

pub fn unique_map<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Unique<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for Unique<T> {
        type Value = BTreeMap<String, T>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("unique named values")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut result = BTreeMap::new();
            while let Some((k, v)) = map.next_entry::<String, T>()? {
                if result.insert(k, v).is_some() {
                    return Err(A::Error::custom("duplicate key"));
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(Unique(std::marker::PhantomData))
}

impl WireAnswer {
    pub fn checked_for(self, question: &Question) -> Result<Answer, DecisionError> {
        let answer = match (self, question) {
            (Self::Noul { noul }, Question::Noul { .. }) => Answer {
                noul: Some(noul),
                ..Default::default()
            },
            (
                Self::Choice {
                    choice,
                    probabilities,
                    confidence,
                },
                Question::Choice { .. },
            ) => Answer {
                choice: Some(choice),
                probabilities,
                confidence,
                ..Default::default()
            },
            (
                Self::Score {
                    score,
                    probabilities,
                    legend,
                    confidence,
                },
                Question::Score { criteria, .. },
            ) => {
                if legend
                    != criteria
                        .iter()
                        .enumerate()
                        .map(|(i, s)| (i.to_string(), s.clone()))
                        .collect()
                    || (score
                        - probabilities
                            .iter()
                            .map(|(k, p)| k.parse::<usize>().map_or(f64::NAN, |i| i as f64 * p))
                            .sum::<f64>())
                    .abs()
                        > 0.01
                {
                    return Err(DecisionError::InvalidAnswer);
                }
                Answer {
                    score: Some(score),
                    probabilities,
                    legend,
                    confidence,
                    ..Default::default()
                }
            }
            _ => return Err(DecisionError::InvalidAnswer),
        };
        if !answer.answers(question) {
            return Err(DecisionError::InvalidAnswer);
        }
        Ok(answer)
    }
}
