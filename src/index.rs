use anyhow::Context;
use heed::types::*;
use heed::{PolyDatabase, Database};
use roaring::RoaringBitmap;

use crate::Search;
use crate::{BEU32, DocumentId};
use crate::fields_ids_map::FieldsIdsMap;
use crate::{
    RoaringBitmapCodec, BEU32StrCodec, StrStrU8Codec, ObkvCodec,
    BoRoaringBitmapCodec, CboRoaringBitmapCodec,
};

pub const WORDS_FST_KEY: &str = "words-fst";
pub const FIELDS_IDS_MAP_KEY: &str = "fields-ids-map";
pub const DOCUMENTS_IDS_KEY: &str = "documents-ids";

#[derive(Clone)]
pub struct Index {
    /// Contains many different types (e.g. the fields ids map).
    pub main: PolyDatabase,
    /// A word and all the documents ids containing the word.
    pub word_docids: Database<Str, RoaringBitmapCodec>,
    /// Maps a word and a document id (u32) to all the positions where the given word appears.
    pub docid_word_positions: Database<BEU32StrCodec, BoRoaringBitmapCodec>,
    /// Maps the proximity between a pair of words with all the docids where this relation appears.
    pub word_pair_proximity_docids: Database<StrStrU8Codec, CboRoaringBitmapCodec>,
    /// Maps the document id to the document as an obkv store.
    pub documents: Database<OwnedType<BEU32>, ObkvCodec>,
}

impl Index {
    pub fn new(env: &heed::Env) -> anyhow::Result<Index> {
        Ok(Index {
            main: env.create_poly_database(Some("main"))?,
            word_docids: env.create_database(Some("word-docids"))?,
            docid_word_positions: env.create_database(Some("docid-word-positions"))?,
            word_pair_proximity_docids: env.create_database(Some("word-pair-proximity-docids"))?,
            documents: env.create_database(Some("documents"))?,
        })
    }

    pub fn documents_ids(&self, rtxn: &heed::RoTxn) -> anyhow::Result<Option<RoaringBitmap>> {
        Ok(self.main.get::<_, Str, RoaringBitmapCodec>(rtxn, DOCUMENTS_IDS_KEY)?)
    }

    pub fn put_fields_ids_map(&self, wtxn: &mut heed::RwTxn, map: &FieldsIdsMap) -> heed::Result<()> {
        self.main.put::<_, Str, SerdeJson<FieldsIdsMap>>(wtxn, FIELDS_IDS_MAP_KEY, map)
    }

    pub fn fields_ids_map(&self, rtxn: &heed::RoTxn) -> heed::Result<Option<FieldsIdsMap>> {
        self.main.get::<_, Str, SerdeJson<FieldsIdsMap>>(rtxn, FIELDS_IDS_MAP_KEY)
    }

    pub fn number_of_fields(&self, rtxn: &heed::RoTxn) -> anyhow::Result<Option<usize>> {
        match self.fields_ids_map(rtxn)? {
            Some(map) => Ok(Some(map.len())),
            None => Ok(None),
        }
    }

    pub fn put_fst<A: AsRef<[u8]>>(&self, wtxn: &mut heed::RwTxn, fst: &fst::Set<A>) -> anyhow::Result<()> {
        Ok(self.main.put::<_, Str, ByteSlice>(wtxn, WORDS_FST_KEY, fst.as_fst().as_bytes())?)
    }

    pub fn fst<'t>(&self, rtxn: &'t heed::RoTxn) -> anyhow::Result<Option<fst::Set<&'t [u8]>>> {
        match self.main.get::<_, Str, ByteSlice>(rtxn, WORDS_FST_KEY)? {
            Some(bytes) => Ok(Some(fst::Set::new(bytes)?)),
            None => Ok(None),
        }
    }

    /// Returns a [`Vec`] of the requested documents. Returns an error if a document is missing.
    pub fn documents<'t>(
        &self,
        rtxn: &'t heed::RoTxn,
        ids: impl IntoIterator<Item=DocumentId>,
    ) -> anyhow::Result<Vec<(DocumentId, obkv::KvReader<'t>)>>
    {
        let mut documents = Vec::new();

        for id in ids {
            let kv = self.documents.get(rtxn, &BEU32::new(id))?
                .with_context(|| format!("Could not find document {}", id))?;
            documents.push((id, kv));
        }

        Ok(documents)
    }

    /// Returns the number of documents indexed in the database.
    pub fn number_of_documents(&self, rtxn: &heed::RoTxn) -> anyhow::Result<usize> {
        match self.documents_ids(rtxn)? {
            Some(docids) => Ok(docids.len() as usize),
            None => Ok(0),
        }
    }

    pub fn search<'a>(&'a self, rtxn: &'a heed::RoTxn) -> Search<'a> {
        Search::new(rtxn, self)
    }
}
