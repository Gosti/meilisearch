use std::borrow::Cow;

use anyhow::{bail, ensure};
use bstr::ByteSlice as _;
use fst::IntoStreamer;
use roaring::RoaringBitmap;

use crate::heed_codec::CboRoaringBitmapCodec;

const WORDS_FST_KEY: &[u8] = crate::index::WORDS_FST_KEY.as_bytes();
const FIELDS_IDS_MAP_KEY: &[u8] = crate::index::FIELDS_IDS_MAP_KEY.as_bytes();
const DOCUMENTS_IDS_KEY: &[u8] = crate::index::DOCUMENTS_IDS_KEY.as_bytes();

pub fn main_merge(key: &[u8], values: &[Cow<[u8]>]) -> anyhow::Result<Vec<u8>> {
    match key {
        WORDS_FST_KEY => {
            let fsts: Vec<_> = values.iter().map(|v| fst::Set::new(v).unwrap()).collect();

            // Union of the FSTs
            let mut op = fst::set::OpBuilder::new();
            fsts.iter().for_each(|fst| op.push(fst.into_stream()));
            let op = op.r#union();

            let mut build = fst::SetBuilder::memory();
            build.extend_stream(op.into_stream()).unwrap();
            Ok(build.into_inner().unwrap())
        },
        FIELDS_IDS_MAP_KEY => {
            ensure!(values.windows(2).all(|vs| vs[0] == vs[1]), "fields ids map doesn't match");
            Ok(values[0].to_vec())
        },
        DOCUMENTS_IDS_KEY => word_docids_merge(&[], values),
        otherwise => bail!("wut {:?}", otherwise),
    }
}

pub fn word_docids_merge(_key: &[u8], values: &[Cow<[u8]>]) -> anyhow::Result<Vec<u8>> {
    let (head, tail) = values.split_first().unwrap();
    let mut head = RoaringBitmap::deserialize_from(&head[..])?;

    for value in tail {
        let bitmap = RoaringBitmap::deserialize_from(&value[..])?;
        head.union_with(&bitmap);
    }

    let mut vec = Vec::with_capacity(head.serialized_size());
    head.serialize_into(&mut vec)?;
    Ok(vec)
}

pub fn docid_word_positions_merge(key: &[u8], _values: &[Cow<[u8]>]) -> anyhow::Result<Vec<u8>> {
    bail!("merging docid word positions is an error ({:?})", key.as_bstr())
}

pub fn words_pairs_proximities_docids_merge(_key: &[u8], values: &[Cow<[u8]>]) -> anyhow::Result<Vec<u8>> {
    let (head, tail) = values.split_first().unwrap();
    let mut head = CboRoaringBitmapCodec::deserialize_from(&head[..])?;

    for value in tail {
        let bitmap = CboRoaringBitmapCodec::deserialize_from(&value[..])?;
        head.union_with(&bitmap);
    }

    let mut vec = Vec::new();
    CboRoaringBitmapCodec::serialize_into(&head, &mut vec)?;
    Ok(vec)
}

pub fn documents_merge(key: &[u8], _values: &[Cow<[u8]>]) -> anyhow::Result<Vec<u8>> {
    bail!("merging documents is an error ({:?})", key.as_bstr())
}
