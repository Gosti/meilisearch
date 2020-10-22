use std::collections::{BTreeMap, HashMap};
use std::convert::{TryFrom, TryInto};
use std::fs::File;
use std::io::Read;
use std::iter::FromIterator;
use std::time::Instant;
use std::{cmp, iter};

use anyhow::Context;
use bstr::ByteSlice as _;
use csv::StringRecord;
use heed::BytesEncode;
use linked_hash_map::LinkedHashMap;
use log::{debug, info};
use grenad::{Reader, FileFuse, Writer, Sorter, CompressionType};
use roaring::RoaringBitmap;
use tempfile::tempfile;

use crate::fields_ids_map::FieldsIdsMap;
use crate::heed_codec::{BoRoaringBitmapCodec, CboRoaringBitmapCodec};
use crate::tokenizer::{simple_tokenizer, only_token};
use crate::{SmallVec32, Position, DocumentId};

use super::{MergeFn, create_writer, create_sorter, writer_into_reader};
use super::merge_function::{main_merge, word_docids_merge, words_pairs_proximities_docids_merge};

const LMDB_MAX_KEY_LENGTH: usize = 511;
const ONE_KILOBYTE: usize = 1024 * 1024;

const MAX_POSITION: usize = 1000;
const MAX_ATTRIBUTES: usize = u32::max_value() as usize / MAX_POSITION;

const WORDS_FST_KEY: &[u8] = crate::index::WORDS_FST_KEY.as_bytes();
const FIELDS_IDS_MAP_KEY: &[u8] = crate::index::FIELDS_IDS_MAP_KEY.as_bytes();
const DOCUMENTS_IDS_KEY: &[u8] = crate::index::DOCUMENTS_IDS_KEY.as_bytes();

pub struct Readers {
    pub main: Reader<FileFuse>,
    pub word_docids: Reader<FileFuse>,
    pub docid_word_positions: Reader<FileFuse>,
    pub words_pairs_proximities_docids: Reader<FileFuse>,
    pub documents: Reader<FileFuse>,
}

pub struct Store {
    word_docids: LinkedHashMap<SmallVec32<u8>, RoaringBitmap>,
    word_docids_limit: usize,
    words_pairs_proximities_docids: LinkedHashMap<(SmallVec32<u8>, SmallVec32<u8>, u8), RoaringBitmap>,
    words_pairs_proximities_docids_limit: usize,
    documents_ids: RoaringBitmap,
    // MTBL parameters
    chunk_compression_type: CompressionType,
    chunk_compression_level: Option<u32>,
    chunk_fusing_shrink_size: Option<u64>,
    // MTBL sorters
    main_sorter: Sorter<MergeFn>,
    word_docids_sorter: Sorter<MergeFn>,
    words_pairs_proximities_docids_sorter: Sorter<MergeFn>,
    // MTBL writers
    docid_word_positions_writer: Writer<File>,
    documents_writer: Writer<File>,
}

impl Store {
    pub fn new(
        linked_hash_map_size: usize,
        max_nb_chunks: Option<usize>,
        max_memory: Option<usize>,
        chunk_compression_type: CompressionType,
        chunk_compression_level: Option<u32>,
        chunk_fusing_shrink_size: Option<u64>,
    ) -> anyhow::Result<Store>
    {
        // We divide the max memory by the number of sorter the Store have.
        let max_memory = max_memory.map(|mm| cmp::max(ONE_KILOBYTE, mm / 3));

        let main_sorter = create_sorter(
            main_merge,
            chunk_compression_type,
            chunk_compression_level,
            chunk_fusing_shrink_size,
            max_nb_chunks,
            max_memory,
        );
        let word_docids_sorter = create_sorter(
            word_docids_merge,
            chunk_compression_type,
            chunk_compression_level,
            chunk_fusing_shrink_size,
            max_nb_chunks,
            max_memory,
        );
        let words_pairs_proximities_docids_sorter = create_sorter(
            words_pairs_proximities_docids_merge,
            chunk_compression_type,
            chunk_compression_level,
            chunk_fusing_shrink_size,
            max_nb_chunks,
            max_memory,
        );

        let documents_writer = tempfile().and_then(|f| {
            create_writer(chunk_compression_type, chunk_compression_level, f)
        })?;
        let docid_word_positions_writer = tempfile().and_then(|f| {
            create_writer(chunk_compression_type, chunk_compression_level, f)
        })?;

        Ok(Store {
            word_docids: LinkedHashMap::with_capacity(linked_hash_map_size),
            word_docids_limit: linked_hash_map_size,
            words_pairs_proximities_docids: LinkedHashMap::with_capacity(linked_hash_map_size),
            words_pairs_proximities_docids_limit: linked_hash_map_size,
            documents_ids: RoaringBitmap::new(),
            chunk_compression_type,
            chunk_compression_level,
            chunk_fusing_shrink_size,

            main_sorter,
            word_docids_sorter,
            words_pairs_proximities_docids_sorter,

            docid_word_positions_writer,
            documents_writer,
        })
    }

    // Save the documents ids under the position and word we have seen it.
    fn insert_word_docid(&mut self, word: &str, id: DocumentId) -> anyhow::Result<()> {
        // if get_refresh finds the element it is assured to be at the end of the linked hash map.
        match self.word_docids.get_refresh(word.as_bytes()) {
            Some(old) => { old.insert(id); },
            None => {
                let word_vec = SmallVec32::from(word.as_bytes());
                // A newly inserted element is append at the end of the linked hash map.
                self.word_docids.insert(word_vec, RoaringBitmap::from_iter(Some(id)));
                // If the word docids just reached it's capacity we must make sure to remove
                // one element, this way next time we insert we doesn't grow the capacity.
                if self.word_docids.len() == self.word_docids_limit {
                    // Removing the front element is equivalent to removing the LRU element.
                    let lru = self.word_docids.pop_front();
                    Self::write_word_docids(&mut self.word_docids_sorter, lru)?;
                }
            }
        }
        Ok(())
    }

    // Save the documents ids under the words pairs proximities that it contains.
    fn insert_words_pairs_proximities_docids<'a>(
        &mut self,
        words_pairs_proximities: impl IntoIterator<Item=((&'a str, &'a str), u8)>,
        id: DocumentId,
    ) -> anyhow::Result<()>
    {
        for ((w1, w2), prox) in words_pairs_proximities {
            let w1 = SmallVec32::from(w1.as_bytes());
            let w2 = SmallVec32::from(w2.as_bytes());
            let key = (w1, w2, prox);
            // if get_refresh finds the element it is assured
            // to be at the end of the linked hash map.
            match self.words_pairs_proximities_docids.get_refresh(&key) {
                Some(old) => { old.insert(id); },
                None => {
                    // A newly inserted element is append at the end of the linked hash map.
                    let ids = RoaringBitmap::from_iter(Some(id));
                    self.words_pairs_proximities_docids.insert(key, ids);
                }
            }
        }

        // If the linked hashmap is over capacity we must remove the overflowing elements.
        let len = self.words_pairs_proximities_docids.len();
        let overflow = len.checked_sub(self.words_pairs_proximities_docids_limit);
        if let Some(overflow) = overflow {
            let mut lrus = Vec::with_capacity(overflow);
            // Removing front elements is equivalent to removing the LRUs.
            let iter = iter::from_fn(|| self.words_pairs_proximities_docids.pop_front());
            iter.take(overflow).for_each(|x| lrus.push(x));
            Self::write_words_pairs_proximities(&mut self.words_pairs_proximities_docids_sorter, lrus)?;
        }

        Ok(())
    }

    fn write_fields_ids_map(&mut self, map: &FieldsIdsMap) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(&map)?;
        self.main_sorter.insert(FIELDS_IDS_MAP_KEY, bytes)?;
        Ok(())
    }

    fn write_document(
        &mut self,
        document_id: DocumentId,
        words_positions: &HashMap<String, SmallVec32<Position>>,
        record: &StringRecord,
    ) -> anyhow::Result<()>
    {
        // We compute the list of words pairs proximities (self-join) and write it directly to disk.
        let words_pair_proximities = compute_words_pair_proximities(&words_positions);
        self.insert_words_pairs_proximities_docids(words_pair_proximities, document_id)?;

        // We store document_id associated with all the words the record contains.
        for (word, _) in words_positions {
            self.insert_word_docid(word, document_id)?;
        }

        let mut writer = obkv::KvWriter::memory();
        record.iter().enumerate().for_each(|(i, v)| {
            let key = i.try_into().unwrap();
            writer.insert(key, v.as_bytes()).unwrap();
        });
        let bytes = writer.into_inner().unwrap();

        self.documents_ids.insert(document_id);
        self.documents_writer.insert(document_id.to_be_bytes(), bytes)?;
        Self::write_docid_word_positions(&mut self.docid_word_positions_writer, document_id, words_positions)?;

        Ok(())
    }

    fn write_words_pairs_proximities(
        sorter: &mut Sorter<MergeFn>,
        iter: impl IntoIterator<Item=((SmallVec32<u8>, SmallVec32<u8>, u8), RoaringBitmap)>,
    ) -> anyhow::Result<()>
    {
        let mut key = Vec::new();
        let mut buffer = Vec::new();

        for ((w1, w2, min_prox), docids) in iter {
            key.clear();
            key.extend_from_slice(w1.as_bytes());
            key.push(0);
            key.extend_from_slice(w2.as_bytes());
            // Storing the minimun proximity found between those words
            key.push(min_prox);
            // We serialize the document ids into a buffer
            buffer.clear();
            buffer.reserve(CboRoaringBitmapCodec::serialized_size(&docids));
            CboRoaringBitmapCodec::serialize_into(&docids, &mut buffer)?;
            // that we write under the generated key into MTBL
            if lmdb_key_valid_size(&key) {
                sorter.insert(&key, &buffer)?;
            }
        }

        Ok(())
    }

    fn write_docid_word_positions(
        writer: &mut Writer<File>,
        id: DocumentId,
        words_positions: &HashMap<String, SmallVec32<Position>>,
    ) -> anyhow::Result<()>
    {
        // We prefix the words by the document id.
        let mut key = id.to_be_bytes().to_vec();
        let base_size = key.len();

        // We order the words lexicographically, this way we avoid passing by a sorter.
        let words_positions = BTreeMap::from_iter(words_positions);

        for (word, positions) in words_positions {
            key.truncate(base_size);
            key.extend_from_slice(word.as_bytes());
            // We serialize the positions into a buffer.
            let positions = RoaringBitmap::from_iter(positions.iter().cloned());
            let bytes = BoRoaringBitmapCodec::bytes_encode(&positions)
                .with_context(|| "could not serialize positions")?;
            // that we write under the generated key into MTBL
            if lmdb_key_valid_size(&key) {
                writer.insert(&key, &bytes)?;
            }
        }

        Ok(())
    }

    fn write_word_docids<I>(sorter: &mut Sorter<MergeFn>, iter: I) -> anyhow::Result<()>
    where I: IntoIterator<Item=(SmallVec32<u8>, RoaringBitmap)>
    {
        let mut key = Vec::new();
        let mut buffer = Vec::new();

        for (word, ids) in iter {
            key.clear();
            key.extend_from_slice(&word);
            // We serialize the document ids into a buffer
            buffer.clear();
            let ids = RoaringBitmap::from_iter(ids);
            buffer.reserve(ids.serialized_size());
            ids.serialize_into(&mut buffer)?;
            // that we write under the generated key into MTBL
            if lmdb_key_valid_size(&key) {
                sorter.insert(&key, &buffer)?;
            }
        }

        Ok(())
    }

    fn write_documents_ids(sorter: &mut Sorter<MergeFn>, ids: RoaringBitmap) -> anyhow::Result<()> {
        let mut buffer = Vec::with_capacity(ids.serialized_size());
        ids.serialize_into(&mut buffer)?;
        sorter.insert(DOCUMENTS_IDS_KEY, &buffer)?;
        Ok(())
    }

    pub fn index_csv<'a, F>(
        mut self,
        mut rdr: csv::Reader<Box<dyn Read + Send + 'a>>,
        base_document_id: usize,
        thread_index: usize,
        num_threads: usize,
        log_every_n: usize,
        mut progress_callback: F,
    ) -> anyhow::Result<Readers>
    where F: FnMut(u32),
    {
        debug!("{:?}: Indexing in a Store...", thread_index);

        // Write the headers into the store.
        let headers = rdr.headers()?;

        let mut fields_ids_map = FieldsIdsMap::new();
        for header in headers.iter() {
            fields_ids_map.insert(header).context("no more field id available")?;
        }
        self.write_fields_ids_map(&fields_ids_map)?;

        let mut before = Instant::now();
        let mut document_id: usize = base_document_id;
        let mut document = csv::StringRecord::new();
        let mut words_positions = HashMap::new();

        while rdr.read_record(&mut document)? {
            // We skip documents that must not be indexed by this thread.
            if document_id % num_threads == thread_index {
                // This is a log routine that we do every `log_every_n` documents.
                if document_id % log_every_n == 0 {
                    let count = format_count(document_id);
                    info!("We have seen {} documents so far ({:.02?}).", count, before.elapsed());
                    progress_callback((document_id - base_document_id) as u32);
                    before = Instant::now();
                }

                let document_id = DocumentId::try_from(document_id).context("generated id is too big")?;
                for (attr, content) in document.iter().enumerate().take(MAX_ATTRIBUTES) {
                    for (pos, token) in simple_tokenizer(&content).filter_map(only_token).enumerate().take(MAX_POSITION) {
                        let word = token.to_lowercase();
                        let position = (attr * MAX_POSITION + pos) as u32;
                        words_positions.entry(word).or_insert_with(SmallVec32::new).push(position);
                    }
                }

                // We write the document in the documents store.
                self.write_document(document_id, &words_positions, &document)?;
                words_positions.clear();
            }

            // Compute the document id of the next document.
            document_id = document_id + 1;
        }

        progress_callback((document_id - base_document_id) as u32);

        let readers = self.finish()?;
        debug!("{:?}: Store created!", thread_index);
        Ok(readers)
    }

    fn finish(mut self) -> anyhow::Result<Readers> {
        let comp_type = self.chunk_compression_type;
        let comp_level = self.chunk_compression_level;
        let shrink_size = self.chunk_fusing_shrink_size;

        Self::write_word_docids(&mut self.word_docids_sorter, self.word_docids)?;
        Self::write_documents_ids(&mut self.main_sorter, self.documents_ids)?;
        Self::write_words_pairs_proximities(
            &mut self.words_pairs_proximities_docids_sorter,
            self.words_pairs_proximities_docids,
        )?;

        let mut word_docids_wtr = tempfile().and_then(|f| create_writer(comp_type, comp_level, f))?;
        let mut builder = fst::SetBuilder::memory();

        let mut iter = self.word_docids_sorter.into_iter()?;
        while let Some((word, val)) = iter.next()? {
            // This is a lexicographically ordered word position
            // we use the key to construct the words fst.
            builder.insert(word)?;
            word_docids_wtr.insert(word, val)?;
        }

        let fst = builder.into_set();
        self.main_sorter.insert(WORDS_FST_KEY, fst.as_fst().as_bytes())?;

        let mut main_wtr = tempfile().and_then(|f| create_writer(comp_type, comp_level, f))?;
        self.main_sorter.write_into(&mut main_wtr)?;

        let mut words_pairs_proximities_docids_wtr = tempfile().and_then(|f| create_writer(comp_type, comp_level, f))?;
        self.words_pairs_proximities_docids_sorter.write_into(&mut words_pairs_proximities_docids_wtr)?;

        let main = writer_into_reader(main_wtr, shrink_size)?;
        let word_docids = writer_into_reader(word_docids_wtr, shrink_size)?;
        let words_pairs_proximities_docids = writer_into_reader(words_pairs_proximities_docids_wtr, shrink_size)?;
        let docid_word_positions = writer_into_reader(self.docid_word_positions_writer, shrink_size)?;
        let documents = writer_into_reader(self.documents_writer, shrink_size)?;

        Ok(Readers {
            main,
            word_docids,
            docid_word_positions,
            words_pairs_proximities_docids,
            documents,
        })
    }
}

/// Outputs a list of all pairs of words with the shortest proximity between 1 and 7 inclusive.
///
/// This list is used by the engine to calculate the documents containing words that are
/// close to each other.
fn compute_words_pair_proximities(
    word_positions: &HashMap<String, SmallVec32<Position>>,
) -> HashMap<(&str, &str), u8>
{
    use itertools::Itertools;

    let mut words_pair_proximities = HashMap::new();
    for ((w1, ps1), (w2, ps2)) in word_positions.iter().cartesian_product(word_positions) {
        let mut min_prox = None;
        for (ps1, ps2) in ps1.iter().cartesian_product(ps2) {
            let prox = crate::proximity::positions_proximity(*ps1, *ps2);
            let prox = u8::try_from(prox).unwrap();
            // We don't care about a word that appear at the
            // same position or too far from the other.
            if prox >= 1 && prox <= 7 {
                if min_prox.map_or(true, |mp| prox < mp) {
                    min_prox = Some(prox)
                }
            }
        }

        if let Some(min_prox) = min_prox {
            words_pair_proximities.insert((w1.as_str(), w2.as_str()), min_prox);
        }
    }

    words_pair_proximities
}

fn format_count(n: usize) -> String {
    human_format::Formatter::new().with_decimals(1).with_separator("").format(n as f64)
}

fn lmdb_key_valid_size(key: &[u8]) -> bool {
    !key.is_empty() && key.len() <= LMDB_MAX_KEY_LENGTH
}
