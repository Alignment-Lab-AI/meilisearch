use std::sync::Arc;

use rayon::iter::IndexedParallelIterator;

use super::DocumentChanges;
use crate::documents::{DocumentIdExtractionError, PrimaryKey};
use crate::update::concurrent_available_ids::ConcurrentAvailableIds;
use crate::update::new::parallel_iterator_ext::ParallelIteratorExt;
use crate::update::new::{DocumentChange, Insertion, KvWriterFieldId};
use crate::{all_obkv_to_json, Error, FieldsIdsMap, Object, Result, UserError};

pub struct PartialDump<I> {
    iter: I,
}

impl<I> PartialDump<I> {
    pub fn new_from_jsonlines(iter: I) -> Self {
        PartialDump { iter }
    }
}

impl<'p, I> DocumentChanges<'p> for PartialDump<I>
where
    I: IndexedParallelIterator<Item = Object> + Clone + 'p,
{
    type Parameter = (&'p FieldsIdsMap, &'p ConcurrentAvailableIds, &'p PrimaryKey<'p>);

    /// Note for future self:
    ///   - the field ids map must already be valid so you must have to generate it beforehand.
    ///   - We should probably expose another method that generates the fields ids map from an iterator of JSON objects.
    ///   - We recommend sending chunks of documents in this `PartialDumpIndexer` we therefore need to create a custom take_while_size method (that doesn't drop items).
    fn document_changes(
        self,
        _fields_ids_map: &mut FieldsIdsMap,
        param: Self::Parameter,
    ) -> Result<
        impl IndexedParallelIterator<Item = std::result::Result<DocumentChange, Arc<Error>>>
            + Clone
            + 'p,
    > {
        let (fields_ids_map, concurrent_available_ids, primary_key) = param;

        Ok(self.iter.try_map_try_init(
            || Ok(()),
            |_, object| {
                let docid = match concurrent_available_ids.next() {
                    Some(id) => id,
                    None => return Err(Error::UserError(UserError::DocumentLimitReached)),
                };

                let mut writer = KvWriterFieldId::memory();
                object.iter().for_each(|(key, value)| {
                    let key = fields_ids_map.id(key).unwrap();
                    /// TODO better error management
                    let value = serde_json::to_vec(&value).unwrap();
                    /// TODO it is not ordered
                    writer.insert(key, value).unwrap();
                });

                let document = writer.into_boxed();
                let external_docid = match primary_key.document_id(&document, fields_ids_map)? {
                    Ok(document_id) => Ok(document_id),
                    Err(DocumentIdExtractionError::InvalidDocumentId(user_error)) => {
                        Err(user_error)
                    }
                    Err(DocumentIdExtractionError::MissingDocumentId) => {
                        Err(UserError::MissingDocumentId {
                            primary_key: primary_key.name().to_string(),
                            document: all_obkv_to_json(&document, fields_ids_map)?,
                        })
                    }
                    Err(DocumentIdExtractionError::TooManyDocumentIds(_)) => {
                        Err(UserError::TooManyDocumentIds {
                            primary_key: primary_key.name().to_string(),
                            document: all_obkv_to_json(&document, fields_ids_map)?,
                        })
                    }
                }?;

                let insertion = Insertion::create(docid, document);
                Ok(DocumentChange::Insertion(insertion))
            },
        ))
    }
}
