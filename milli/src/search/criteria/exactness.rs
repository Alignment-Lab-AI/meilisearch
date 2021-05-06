use std::{collections::HashMap, mem};

use log::debug;
use roaring::RoaringBitmap;
use itertools::Itertools;
use std::ops::BitOr;

use crate::search::query_tree::{Operation, PrimitiveQueryPart};
use crate::search::criteria::{
    Context,
    Criterion,
    CriterionParameters,
    CriterionResult,
    resolve_query_tree,
};
use crate::TreeLevel;

pub struct Exactness<'t> {
    ctx: &'t dyn Context<'t>,
    query_tree: Option<Operation>,
    state: Option<State>,
    bucket_candidates: RoaringBitmap,
    parent: Box<dyn Criterion + 't>,
    query: Vec<ExactQueryPart>,
}

impl<'t> Exactness<'t> {
    pub fn new(ctx: &'t dyn Context<'t>, parent: Box<dyn Criterion + 't>, primitive_query: &[PrimitiveQueryPart]) -> heed::Result<Self> {
        let mut query: Vec<_> = Vec::with_capacity(primitive_query.len());
        for part in primitive_query {
            query.push(ExactQueryPart::from_primitive_query_part(ctx, part)?);
        }

        Ok(Exactness {
            ctx,
            query_tree: None,
            state: None,
            bucket_candidates: RoaringBitmap::new(),
            parent,
            query,
        })
    }
}

impl<'t> Criterion for Exactness<'t> {
    #[logging_timer::time("Exactness::{}")]
    fn next(&mut self, params: &mut CriterionParameters) -> anyhow::Result<Option<CriterionResult>> {
        // remove excluded candidates when next is called, instead of doing it in the loop.
        if let Some(state) = self.state.as_mut() {
            state.difference_with(params.excluded_candidates);
        }

        loop {
            debug!("Exactness at state {:?}", self.state);

            match self.state.as_mut() {
                Some(state) if state.is_empty() => {
                    // reset state
                    self.state = None;
                    self.query_tree = None;
                },
                Some(state) => {
                    let (candidates, state) = resolve_state(self.ctx, mem::take(state), &self.query)?;
                    self.state = state;

                    return Ok(Some(CriterionResult {
                        query_tree: self.query_tree.clone(),
                        candidates: Some(candidates),
                        bucket_candidates: mem::take(&mut self.bucket_candidates),
                    }));
                },
                None => {
                    match self.parent.next(params)? {
                        Some(CriterionResult { query_tree: Some(query_tree), candidates, bucket_candidates }) => {
                            let candidates = match candidates {
                                Some(candidates) => candidates,
                                None => resolve_query_tree(self.ctx, &query_tree, &mut HashMap::new(), params.wdcache)?,
                            };
                            self.state = Some(State::new(candidates));
                            self.query_tree = Some(query_tree);
                            self.bucket_candidates |= bucket_candidates;
                        },
                        Some(CriterionResult { query_tree, candidates, bucket_candidates }) => {
                            return Ok(Some(CriterionResult {
                                query_tree,
                                candidates,
                                bucket_candidates,
                            }));
                        },
                        None => return Ok(None),
                    }
                },
            }
         }
    }
}

#[derive(Debug)]
enum State {
    /// Extract the documents that have an attribute that contains exactly the query.
    ExactAttribute(RoaringBitmap),
    /// Extract the documents that have an attribute that starts with exactly the query.
    AttributeStartsWith(RoaringBitmap),
    /// Rank the remaining documents by the number of exact words contained.
    ExactWords(RoaringBitmap),
    Remainings(Vec<RoaringBitmap>),
}

impl State {
    fn new(candidates: RoaringBitmap) -> Self {
        Self::ExactAttribute(candidates)
    }

    fn difference_with(&mut self, lhs: &RoaringBitmap) {
        match self {
            Self::ExactAttribute(candidates) |
            Self::AttributeStartsWith(candidates) |
            Self::ExactWords(candidates) => *candidates -= lhs,
            Self::Remainings(candidates_array) => {
                candidates_array.iter_mut().for_each(|candidates| *candidates -= lhs);
                candidates_array.retain(|candidates| !candidates.is_empty());
            }
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::ExactAttribute(candidates) |
            Self::AttributeStartsWith(candidates) |
            Self::ExactWords(candidates) => candidates.is_empty(),
            Self::Remainings(candidates_array) => {
                candidates_array.iter().all(RoaringBitmap::is_empty)
            }
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::Remainings(vec![])
    }
}

#[logging_timer::time("Exactness::{}")]
fn resolve_state(
    ctx: &dyn Context,
    state: State,
    query: &[ExactQueryPart],
) -> anyhow::Result<(RoaringBitmap, Option<State>)>
{
    use State::*;
    match state {
        ExactAttribute(mut allowed_candidates) => {
            let query_len = query.len() as u32;
            let mut candidates = RoaringBitmap::new();
            let attributes_ids = ctx.searchable_fields_ids()?;
            for id in attributes_ids {
                if let Some(attribute_allowed_docids) = ctx.field_id_len_docids(id, query_len)? {
                    let mut attribute_candidates_array = attribute_start_with_docids(ctx, id as u32, query)?;
                    attribute_candidates_array.push(attribute_allowed_docids);
                    candidates |= intersection_of(attribute_candidates_array.iter().collect());
                }
            }

            // only keep allowed candidates
            candidates &= &allowed_candidates;
            // remove current candidates from allowed candidates
            allowed_candidates -= &candidates;
            Ok((candidates, Some(AttributeStartsWith(allowed_candidates))))

        },
        AttributeStartsWith(mut allowed_candidates) => {
            let mut candidates = RoaringBitmap::new();
            let attributes_ids = ctx.searchable_fields_ids()?;
            for id in attributes_ids {
                let attribute_candidates_array = attribute_start_with_docids(ctx, id as u32, query)?;
                candidates |= intersection_of(attribute_candidates_array.iter().collect());
            }

            // only keep allowed candidates
            candidates &= &allowed_candidates;
            // remove current candidates from allowed candidates
            allowed_candidates -= &candidates;
            Ok((candidates, Some(ExactWords(allowed_candidates))))
        },
        ExactWords(mut allowed_candidates) => {
            let number_of_part = query.len();
            let mut parts_candidates_array = Vec::with_capacity(number_of_part);

            for part in query {
                let mut candidates = RoaringBitmap::new();
                use ExactQueryPart::*;
                match part {
                    Synonyms(synonyms) => {
                        for synonym in synonyms {
                            if let Some(synonym_candidates) = ctx.word_docids(synonym)? {
                                candidates |= synonym_candidates;
                            }
                        }
                    },
                    // compute intersection on pair of words with a proximity of 0.
                    Phrase(phrase) => {
                        let mut bitmaps = Vec::with_capacity(phrase.len().saturating_sub(1));
                        for words in phrase.windows(2) {
                            if let [left, right] = words {
                                match ctx.word_pair_proximity_docids(left, right, 0)? {
                                    Some(docids) => bitmaps.push(docids),
                                    None => {
                                        bitmaps.clear();
                                        break
                                    },
                                }
                            }
                        }
                        candidates |= intersection_of(bitmaps.iter().collect());
                    }
                }
                parts_candidates_array.push(candidates);
            }

            let mut candidates_array = Vec::new();

            // compute documents that contain all exact words.
            let mut all_exact_candidates = intersection_of(parts_candidates_array.iter().collect());
            all_exact_candidates &= &allowed_candidates;
            allowed_candidates -= &all_exact_candidates;

            // push the result of combinations of exact words grouped by the number of exact words contained by documents.
            for c_count in (1..number_of_part).rev() {
                let mut combinations_candidates = parts_candidates_array
                    .iter()
                    // create all `c_count` combinations of exact words
                    .combinations(c_count)
                    // intersect each word candidates in combinations
                    .map(intersection_of)
                    // union combinations of `c_count` exact words
                    .fold(RoaringBitmap::new(),  RoaringBitmap::bitor);
                // only keep allowed candidates
                combinations_candidates &= &allowed_candidates;
                // remove current candidates from allowed candidates
                allowed_candidates -= &combinations_candidates;
                candidates_array.push(combinations_candidates);
            }

            // push remainings allowed candidates as the worst valid candidates
            candidates_array.push(allowed_candidates);
            // reverse the array to be able to pop candidates from the best to the worst.
            candidates_array.reverse();

            Ok((all_exact_candidates, Some(Remainings(candidates_array))))
        },
        // pop remainings candidates until the emptiness
        Remainings(mut candidates_array) => {
            let candidates = candidates_array.pop().unwrap_or_default();
            if !candidates_array.is_empty() {
                Ok((candidates, Some(Remainings(candidates_array))))
            } else {
                Ok((candidates, None))
            }
        },

    }
}

fn attribute_start_with_docids(ctx: &dyn Context, attribute_id: u32, query: &[ExactQueryPart]) -> heed::Result<Vec<RoaringBitmap>> {
    let lowest_level = TreeLevel::min_value();
    let mut attribute_candidates_array = Vec::new();
    // start from attribute first position
    let mut pos = attribute_id * 1000;
    for part in query {
        use ExactQueryPart::*;
        match part {
            Synonyms(synonyms) => {
                let mut synonyms_candidates = RoaringBitmap::new();
                for word in synonyms {
                    let wc = ctx.word_level_position_docids(word, lowest_level, pos, pos)?;
                    if let Some(word_candidates) = wc {
                        synonyms_candidates |= word_candidates;
                    }
                }
                attribute_candidates_array.push(synonyms_candidates);
                pos += 1;
            },
            Phrase(phrase) => {
                for word in phrase {
                    let wc = ctx.word_level_position_docids(word, lowest_level, pos, pos)?;
                    if let Some(word_candidates) = wc {
                        attribute_candidates_array.push(word_candidates);
                    }
                    pos += 1;
                }
            }
        }
    }

    Ok(attribute_candidates_array)
}

fn intersection_of(mut rbs: Vec<&RoaringBitmap>) -> RoaringBitmap {
    rbs.sort_unstable_by_key(|rb| rb.len());
    let mut iter = rbs.into_iter();
    match iter.next() {
        Some(first) => iter.fold(first.clone(), |acc, rb| acc & rb),
        None => RoaringBitmap::new(),
    }
}

#[derive(Debug, Clone)]
pub enum ExactQueryPart {
    Phrase(Vec<String>),
    Synonyms(Vec<String>),
}

impl ExactQueryPart {
    fn from_primitive_query_part(ctx: &dyn Context, part: &PrimitiveQueryPart) -> heed::Result<Self> {
        let part = match part {
            PrimitiveQueryPart::Word(word, _) => {
                match ctx.synonyms(word)? {
                    Some(synonyms) => {
                        let mut synonyms: Vec<_> = synonyms.into_iter().filter_map(|mut array| {
                            // keep 1 word synonyms only.
                            match array.pop() {
                                Some(word) if array.is_empty() => Some(word),
                                _ => None,
                            }
                        }).collect();
                        synonyms.push(word.clone());
                        ExactQueryPart::Synonyms(synonyms)
                    },
                    None => ExactQueryPart::Synonyms(vec![word.clone()]),
                }
            },
            PrimitiveQueryPart::Phrase(phrase) => ExactQueryPart::Phrase(phrase.clone()),
        };

        Ok(part)
    }
}
