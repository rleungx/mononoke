// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use std::fmt::Debug;
use std::mem;

use bytes::Bytes;
use failure::Compat;
use futures::{Future, Poll, Stream};
use futures::future::Shared;
use futures_ext::{BoxFuture, FutureExt};

use blobrepo::{BlobEntry, BlobRepo};
use mercurial::manifest::revlog::ManifestContent;
use mercurial_bundles::wirepack::{DataEntry, HistoryEntry, Part};
use mercurial_bundles::wirepack::converter::{WirePackConverter, WirePackPartProcessor};
use mercurial_types::{delta, manifest, Blob, NodeHash, RepoPath, NULL_HASH};

use errors::*;
use upload_blobs::UploadableBlob;

/// Parser for wirepack tree part. It returns a stream of TreemanifestEntry, that can be used by
/// Mononoke's Commit Api.
///
/// It assumes a few things:
/// 1) all data is sent as a delta from the null revision (i.e. data is basically non-deltaed).
/// 2) there are exactly one history entry and exactly one data entry for each tree.
pub struct TreemanifestBundle2Parser<S> {
    stream: WirePackConverter<S, TreemanifestPartProcessor>,
}

impl<S> TreemanifestBundle2Parser<S>
where
    S: Stream<Item = Part, Error = Error>,
{
    pub fn new(part_stream: S) -> Self {
        Self {
            stream: WirePackConverter::new(part_stream, TreemanifestPartProcessor::new()),
        }
    }
}

impl<S> Stream for TreemanifestBundle2Parser<S>
where
    S: Stream<Item = Part, Error = Error>,
{
    type Item = TreemanifestEntry;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Error> {
        self.stream.poll()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct TreemanifestEntry {
    pub node: NodeHash,
    pub data: Bytes,
    pub p1: Option<NodeHash>,
    pub p2: Option<NodeHash>,
    pub path: RepoPath,
    pub manifest_content: ManifestContent,
}

impl TreemanifestEntry {
    fn new(
        node: NodeHash,
        data: Bytes,
        p1: NodeHash,
        p2: NodeHash,
        path: RepoPath,
    ) -> Result<Self> {
        let manifest_content = ManifestContent::parse(data.as_ref())?;

        Ok(Self {
            node,
            data,
            p1: p1.into_option(),
            p2: p2.into_option(),
            path,
            manifest_content,
        })
    }
}

impl UploadableBlob for TreemanifestEntry {
    type Value = (
        ManifestContent,
        Shared<BoxFuture<(BlobEntry, RepoPath), Compat<Error>>>,
    );

    fn upload(self, repo: &BlobRepo) -> Result<((NodeHash, RepoPath), Self::Value)> {
        let path = self.path;
        let manifest_content = self.manifest_content;
        repo.upload_entry(
            Blob::from(self.data),
            manifest::Type::Tree,
            self.p1,
            self.p2,
            path.clone(),
        ).map(move |(node, value)| {
            (
                (node, path),
                (
                    manifest_content,
                    value.map_err(Error::compat).boxify().shared(),
                ),
            )
        })
    }
}

struct TreemanifestPartProcessor {
    node: Option<NodeHash>,
    p1: Option<NodeHash>,
    p2: Option<NodeHash>,
    path: Option<RepoPath>,
}

impl TreemanifestPartProcessor {
    fn new() -> Self {
        Self {
            node: None,
            p1: None,
            p2: None,
            path: None,
        }
    }
}

impl WirePackPartProcessor for TreemanifestPartProcessor {
    type Data = TreemanifestEntry;

    fn history_meta(&mut self, path: &RepoPath, entry_count: u32) -> Result<Option<Self::Data>> {
        replace_or_fail_if_exists(&mut self.path, path.clone())?;
        if entry_count != 1 {
            let msg = format!("expected exactly one history entry, got: {}", entry_count);
            return Err(ErrorKind::MalformedTreemanifestPart(msg).into());
        }
        Ok(None)
    }

    fn history(&mut self, entry: &HistoryEntry) -> Result<Option<Self::Data>> {
        replace_or_fail_if_exists(&mut self.node, entry.node.clone())?;
        replace_or_fail_if_exists(&mut self.p1, entry.p1.clone())?;
        replace_or_fail_if_exists(&mut self.p2, entry.p2.clone())?;
        Ok(None)
    }

    fn data_meta(&mut self, path: &RepoPath, entry_count: u32) -> Result<Option<Self::Data>> {
        if Some(path) != self.path.as_ref() {
            let msg = format!("unexpected path: {:?} != {:?}", path, self.path);
            Err(ErrorKind::MalformedTreemanifestPart(msg).into())
        } else if entry_count != 1 {
            let msg = format!("expected exactly one data entry, got: {}", entry_count);
            Err(ErrorKind::MalformedTreemanifestPart(msg).into())
        } else {
            Ok(None)
        }
    }

    fn data(&mut self, data_entry: &DataEntry) -> Result<Option<Self::Data>> {
        if data_entry.delta_base != NULL_HASH {
            let msg = format!("unexpected delta base: {:?}", data_entry.delta_base);
            return Err(ErrorKind::MalformedTreemanifestPart(msg).into());
        }

        let node = unwrap_field(&mut self.node, "node")?;
        let bytes = Bytes::from(delta::apply("".as_bytes(), &data_entry.delta));
        let p1 = unwrap_field(&mut self.p1, "p1")?;
        let p2 = unwrap_field(&mut self.p2, "p2")?;
        let path = unwrap_field(&mut self.path, "path")?;

        Ok(Some(TreemanifestEntry::new(node, bytes, p1, p2, path)?))
    }

    fn end(&mut self) -> Result<Option<Self::Data>> {
        Ok(None)
    }
}

fn replace_or_fail_if_exists<T: Debug>(existing: &mut Option<T>, new_value: T) -> Result<()> {
    let existing = mem::replace(existing, Some(new_value));
    if !existing.is_none() {
        let msg = format!("{:?} was already set", existing);
        Err(ErrorKind::MalformedTreemanifestPart(msg).into())
    } else {
        Ok(())
    }
}

fn unwrap_field<T: Clone>(field: &mut Option<T>, field_name: &str) -> Result<T> {
    field.take().ok_or_else(|| {
        let msg = format!("{} is not set", field_name);
        ErrorKind::MalformedTreemanifestPart(msg).into()
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use futures::{stream, Future};

    use mercurial::manifest::revlog::Details;
    use mercurial_types::{EntryId, MPath};
    use mercurial_types::manifest::Type;
    use mercurial_types_mocks::nodehash as nodehash_mocks;

    #[test]
    fn test_simple() {
        let parts = vec![
            get_history_meta(),
            get_history_entry(),
            get_data_meta(),
            get_data_entry(),
            get_history_meta(),
            get_history_entry(),
            get_data_meta(),
            get_data_entry(),
            Part::End,
        ];

        let part_stream = stream::iter_ok(parts.into_iter());
        let stream = TreemanifestBundle2Parser::new(part_stream);
        assert_eq!(
            stream.collect().wait().unwrap(),
            vec![get_expected_entry(), get_expected_entry()]
        );
    }

    #[test]
    fn test_broken() {
        let parts = vec![get_history_meta(), get_history_entry(), Part::End];
        assert_fails(parts);
        let parts = vec![
            get_history_meta(),
            get_history_entry(),
            get_data_meta(),
            Part::End,
        ];
        assert_fails(parts);
        let parts = vec![
            get_history_meta(),
            get_history_entry(),
            get_data_entry(),
            get_data_meta(),
            Part::End,
        ];
        assert_fails(parts);

        let parts = vec![
            get_history_meta(),
            get_history_entry(),
            Part::DataMeta {
                path: RepoPath::dir("dir").unwrap(),
                entry_count: 1,
            },
            get_data_entry(),
            Part::End,
        ];
        assert_fails(parts);
    }

    fn get_history_meta() -> Part {
        Part::HistoryMeta {
            path: RepoPath::root(),
            entry_count: 1,
        }
    }

    fn get_history_entry() -> Part {
        let node = nodehash_mocks::ONES_HASH;
        let p1 = nodehash_mocks::TWOS_HASH;
        let p2 = nodehash_mocks::THREES_HASH;
        let linknode = nodehash_mocks::FOURS_HASH;

        Part::History(HistoryEntry {
            node,
            p1,
            p2,
            linknode,
            copy_from: None,
        })
    }

    fn get_data_meta() -> Part {
        Part::DataMeta {
            path: RepoPath::root(),
            entry_count: 1,
        }
    }

    fn get_revlog_manifest_content() -> ManifestContent {
        ManifestContent {
            files: btreemap!{
                MPath::new("test_dir/test_file").unwrap() =>
                Details::new(
                    EntryId::new(nodehash_mocks::ONES_HASH),
                    Type::File,
                ),
                MPath::new("test_dir2/test_manifest").unwrap() =>
                Details::new(
                    EntryId::new(nodehash_mocks::TWOS_HASH),
                    Type::Tree,
                ),
            },
        }
    }

    fn get_data_entry() -> Part {
        let node = nodehash_mocks::ONES_HASH;

        let data = {
            let mut data = Vec::new();
            get_revlog_manifest_content().generate(&mut data).unwrap();
            data
        };

        Part::Data(DataEntry {
            node,
            delta_base: NULL_HASH,
            delta: delta::Delta::new_fulltext(data),
        })
    }

    fn assert_fails(parts: Vec<Part>) {
        let part_stream = stream::iter_ok(parts.into_iter());
        let stream = TreemanifestBundle2Parser::new(part_stream);
        assert!(stream.collect().wait().is_err());
    }

    fn get_expected_entry() -> TreemanifestEntry {
        let node = nodehash_mocks::ONES_HASH;
        let p1 = nodehash_mocks::TWOS_HASH;
        let p2 = nodehash_mocks::THREES_HASH;

        let data = {
            let mut data = Vec::new();
            get_revlog_manifest_content().generate(&mut data).unwrap();
            data
        };

        let entry =
            TreemanifestEntry::new(node, Bytes::from(data), p1, p2, RepoPath::root()).unwrap();

        assert_eq!(
            entry.manifest_content,
            get_revlog_manifest_content(),
            "Sanity check for manifest content failed"
        );

        entry
    }
}
