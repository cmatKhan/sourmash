pub mod disk_revindex;
pub mod mem_revindex;

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use byteorder::{LittleEndian, WriteBytesExt};
use enum_dispatch::enum_dispatch;
use getset::{Getters, Setters};
use nohash_hasher::BuildNoHashHasher;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};

use crate::collection::CollectionSet;
use crate::encodings::{Color, Colors, Idx};
use crate::index::{GatherResult, SigCounter};
use crate::prelude::*;
use crate::signature::Signature;
use crate::sketch::minhash::KmerMinHash;
use crate::sketch::Sketch;
use crate::storage::rocksdb::{db_options, COLORS, DB};
use crate::HashIntoType;
use crate::Result;

// DB metadata saved in the METADATA column family
const MANIFEST: &str = "manifest";
const STORAGE_SPEC: &str = "storage_spec";
const VERSION: &str = "version";
const PROCESSED: &str = "processed";

type QueryColors = HashMap<Color, Datasets>;
type HashToColorT = HashMap<HashIntoType, Color, BuildNoHashHasher<HashIntoType>>;
#[derive(Serialize, Deserialize)]
pub struct HashToColor(HashToColorT);

#[enum_dispatch(RevIndexOps)]
pub enum RevIndex {
    //Color(color_revindex::ColorRevIndex),
    Plain(disk_revindex::RevIndex),
    //Mem(mem_revindex::RevIndex),
}

#[enum_dispatch]
pub trait RevIndexOps {
    /* TODO: need the repair_cf variant, not available in rocksdb-rust yet
        pub fn repair(index: &Path, colors: bool);
    */

    fn counter_for_query(&self, query: &KmerMinHash) -> SigCounter;

    fn matches_from_counter(&self, counter: SigCounter, threshold: usize) -> Vec<(String, usize)>;

    fn prepare_gather_counters(
        &self,
        query: &KmerMinHash,
    ) -> (SigCounter, QueryColors, HashToColor);

    fn update(self, collection: CollectionSet) -> Result<RevIndex>
    where
        Self: Sized;

    fn compact(&self);

    fn flush(&self) -> Result<()>;

    fn convert(&self, output_db: RevIndex) -> Result<()>;

    fn check(&self, quick: bool) -> DbStats;

    fn gather(
        &self,
        counter: SigCounter,
        query_colors: QueryColors,
        hash_to_color: HashToColor,
        threshold: usize,
        query: &KmerMinHash,
        selection: Option<Selection>,
    ) -> Result<Vec<GatherResult>>;

    fn collection(&self) -> &CollectionSet;

    fn internalize_storage(&mut self) -> Result<()>;
}

impl HashToColor {
    fn new() -> Self {
        HashToColor(HashMap::<
            HashIntoType,
            Color,
            BuildNoHashHasher<HashIntoType>,
        >::with_hasher(BuildNoHashHasher::default()))
    }

    fn get(&self, hash: &HashIntoType) -> Option<&Color> {
        self.0.get(hash)
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn add_to(&mut self, colors: &mut Colors, dataset_id: Idx, matched_hashes: Vec<u64>) {
        let mut color = None;

        matched_hashes.into_iter().for_each(|hash| {
            color = Some(colors.update(color, &[dataset_id]).unwrap());
            self.0.insert(hash, color.unwrap());
        });
    }

    fn reduce_hashes_colors(
        a: (HashToColor, Colors),
        b: (HashToColor, Colors),
    ) -> (HashToColor, Colors) {
        let ((small_hashes, small_colors), (mut large_hashes, mut large_colors)) =
            if a.0.len() > b.0.len() {
                (b, a)
            } else {
                (a, b)
            };

        small_hashes.0.into_iter().for_each(|(hash, color)| {
            large_hashes
                .0
                .entry(hash)
                .and_modify(|entry| {
                    // Hash is already present.
                    // Update the current color by adding the indices from
                    // small_colors.
                    let ids = small_colors.indices(&color);
                    let new_color = large_colors.update(Some(*entry), ids).unwrap();
                    *entry = new_color;
                })
                .or_insert_with(|| {
                    // In this case, the hash was not present yet.
                    // we need to create the same color from small_colors
                    // into large_colors.
                    let ids = small_colors.indices(&color);
                    let new_color = large_colors.update(None, ids).unwrap();
                    assert_eq!(new_color, color);
                    new_color
                });
        });

        (large_hashes, large_colors)
    }
}

impl FromIterator<(HashIntoType, Color)> for HashToColor {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (HashIntoType, Color)>,
    {
        HashToColor(HashToColorT::from_iter(iter))
    }
}

impl RevIndex {
    /* TODO: need the repair_cf variant, not available in rocksdb-rust yet
        pub fn repair(index: &Path, colors: bool) {
            if colors {
                color_revindex::repair(index);
            } else {
                disk_revindex::repair(index);
            }
        }
    */

    pub fn create<P: AsRef<Path>>(
        index: P,
        collection: CollectionSet,
        colors: bool,
    ) -> Result<Self> {
        if colors {
            todo!() //color_revindex::ColorRevIndex::create(index)
        } else {
            disk_revindex::RevIndex::create(index.as_ref(), collection)
        }
    }

    pub fn open<P: AsRef<Path>>(index: P, read_only: bool, spec: Option<&str>) -> Result<Self> {
        let opts = db_options();
        let cfs = DB::list_cf(&opts, index.as_ref())?;

        if cfs.into_iter().any(|c| c == COLORS) {
            // TODO: ColorRevIndex can't be read-only for now,
            //       due to pending unmerged colors
            todo!() //color_revindex::ColorRevIndex::open(index, false)
        } else {
            disk_revindex::RevIndex::open(index, read_only, spec)
        }
    }
}

pub fn prepare_query(search_sig: Signature, selection: &Selection) -> Option<KmerMinHash> {
    let sig = search_sig.select(selection).ok();

    sig.and_then(|sig| {
        if let Sketch::MinHash(mh) = sig.sketches().swap_remove(0) {
            Some(mh)
        } else {
            None
        }
    })
}

#[derive(Debug, Default, Clone)]
pub enum Datasets {
    #[default]
    Empty,
    Unique(Idx),
    Many(RoaringBitmap),
}

impl Hash for Datasets {
    fn hash<H>(&self, state: &mut H)
    where
        H: Hasher,
    {
        match self {
            Self::Empty => todo!(),
            Self::Unique(v) => v.hash(state),
            Self::Many(v) => {
                for value in v.iter() {
                    value.hash(state);
                }
            }
        }
    }
}

impl IntoIterator for Datasets {
    type Item = Idx;
    type IntoIter = Box<dyn Iterator<Item = Self::Item>>;

    fn into_iter(self) -> Self::IntoIter {
        match self {
            Self::Empty => Box::new(std::iter::empty()),
            Self::Unique(v) => Box::new(std::iter::once(v)),
            Self::Many(v) => Box::new(v.into_iter()),
        }
    }
}

impl Extend<Idx> for Datasets {
    fn extend<T>(&mut self, iter: T)
    where
        T: IntoIterator<Item = Idx>,
    {
        if let Self::Many(v) = self {
            v.extend(iter);
            return;
        }

        let mut it = iter.into_iter();
        while let Some(value) = it.next() {
            match self {
                Self::Empty => *self = Datasets::Unique(value),
                Self::Unique(v) => {
                    if *v != value {
                        *self = Self::Many([*v, value].iter().copied().collect());
                    }
                }
                Self::Many(v) => {
                    v.extend(it);
                    return;
                }
            }
        }
    }
}

impl Datasets {
    fn new(vals: &[Idx]) -> Self {
        if vals.is_empty() {
            Self::Empty
        } else if vals.len() == 1 {
            Self::Unique(vals[0])
        } else {
            Self::Many(RoaringBitmap::from_sorted_iter(vals.iter().copied()).unwrap())
        }
    }

    fn from_slice(slice: &[u8]) -> Option<Self> {
        use byteorder::ReadBytesExt;

        if slice.len() == 8 {
            // Unique
            Some(Self::Unique(
                (&slice[..]).read_u32::<LittleEndian>().unwrap(),
            ))
        } else if slice.len() == 1 {
            // Empty
            Some(Self::Empty)
        } else {
            // Many
            Some(Self::Many(RoaringBitmap::deserialize_from(slice).unwrap()))
        }
    }

    fn as_bytes(&self) -> Option<Vec<u8>> {
        match self {
            Self::Empty => Some(vec![42_u8]),
            Self::Unique(v) => {
                let mut buf = vec![0u8; 8];
                (&mut buf[..])
                    .write_u32::<LittleEndian>(*v)
                    .expect("error writing bytes");
                Some(buf)
            }
            Self::Many(v) => {
                let mut buf = vec![];
                v.serialize_into(&mut buf).unwrap();
                Some(buf)
            }
        }
    }

    fn union(&mut self, other: Datasets) {
        match self {
            Datasets::Empty => match other {
                Datasets::Empty => (),
                Datasets::Unique(_) | Datasets::Many(_) => *self = other,
            },
            Datasets::Unique(v) => match other {
                Datasets::Empty => (),
                Datasets::Unique(o) => {
                    if *v != o {
                        *self = Datasets::Many([*v, o].iter().copied().collect())
                    }
                }
                Datasets::Many(mut o) => {
                    o.extend([*v]);
                    *self = Datasets::Many(o);
                }
            },
            Datasets::Many(ref mut v) => v.extend(other),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Unique(_) => 1,
            Self::Many(ref v) => v.len() as usize,
        }
    }

    fn contains(&self, value: &Idx) -> bool {
        match self {
            Self::Empty => false,
            Self::Unique(v) => v == value,
            Self::Many(ref v) => v.contains(*value),
        }
    }
}

#[derive(Getters, Setters, Debug)]
pub struct DbStats {
    #[getset(get = "pub")]
    total_datasets: usize,

    #[getset(get = "pub")]
    total_keys: usize,

    #[getset(get = "pub")]
    kcount: usize,

    #[getset(get = "pub")]
    vcount: usize,

    #[getset(get = "pub")]
    vcounts: histogram::Histogram,
}

fn stats_for_cf(db: Arc<DB>, cf_name: &str, deep_check: bool, quick: bool) -> DbStats {
    use byteorder::ReadBytesExt;
    use histogram::Histogram;

    let cf = db.cf_handle(cf_name).unwrap();

    let iter = db.iterator_cf(&cf, rocksdb::IteratorMode::Start);
    let mut kcount = 0;
    let mut vcount = 0;
    // Using power values from https://docs.rs/histogram/0.8.3/histogram/struct.Config.html#resulting-size
    let mut vcounts = Histogram::new(12, 64).expect("Error initializing histogram");
    let mut datasets: Datasets = Default::default();

    for result in iter {
        let (key, value) = result.unwrap();
        let _k = (&key[..]).read_u64::<LittleEndian>().unwrap();
        kcount += key.len();

        //println!("Saw {} {:?}", k, Datasets::from_slice(&value));
        vcount += value.len();

        if !quick && deep_check {
            let v = Datasets::from_slice(&value).expect("Error with value");
            vcounts.increment(v.len() as u64).unwrap();
            datasets.union(v);
        }
        //println!("Saw {} {:?}", k, value);
    }

    DbStats {
        total_datasets: datasets.len(),
        total_keys: kcount / 8,
        kcount,
        vcount,
        vcounts,
    }
}

#[cfg(test)]
mod test {
    use camino::Utf8PathBuf as PathBuf;
    use tempfile::TempDir;

    use crate::collection::Collection;
    use crate::prelude::*;
    use crate::selection::Selection;
    use crate::storage::{InnerStorage, RocksDBStorage};
    use crate::Result;

    use super::{prepare_query, RevIndex, RevIndexOps};

    #[test]
    fn revindex_index() -> Result<()> {
        let mut basedir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        basedir.push("../../tests/test-data/scaled/");

        let siglist: Vec<_> = (10..=12)
            .map(|i| {
                let mut filename = basedir.clone();
                filename.push(format!("genome-s{}.fa.gz.sig", i));
                filename
            })
            .collect();

        let selection = Selection::builder().ksize(31).scaled(10000).build();
        let output = TempDir::new()?;

        let mut query = None;
        let query_sig = Signature::from_path(&siglist[0])?
            .swap_remove(0)
            .select(&selection)?;
        if let Some(q) = prepare_query(query_sig, &selection) {
            query = Some(q);
        }
        let query = query.unwrap();

        let collection = Collection::from_paths(&siglist)?.select(&selection)?;
        let index = RevIndex::create(output.path(), collection.try_into()?, false)?;

        let counter = index.counter_for_query(&query);
        let matches = index.matches_from_counter(counter, 0);

        assert_eq!(matches, [("../genome-s10.fa.gz".into(), 48)]);

        Ok(())
    }

    #[test]
    fn revindex_update() -> Result<()> {
        let mut basedir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        basedir.push("../../tests/test-data/scaled/");

        let siglist: Vec<_> = (10..=11)
            .map(|i| {
                let mut filename = basedir.clone();
                filename.push(format!("genome-s{}.fa.gz.sig", i));
                filename
            })
            .collect();

        let selection = Selection::builder().ksize(31).scaled(10000).build();
        let output = TempDir::new()?;

        let mut new_siglist = siglist.clone();
        {
            let collection = Collection::from_paths(&siglist)?.select(&selection)?;
            RevIndex::create(output.path(), collection.try_into()?, false)?;
        }

        let mut filename = basedir.clone();
        filename.push("genome-s12.fa.gz.sig");
        new_siglist.push(filename);

        let mut query = None;
        let query_sig = Signature::from_path(&new_siglist[2])?
            .swap_remove(0)
            .select(&selection)?;
        if let Some(q) = prepare_query(query_sig, &selection) {
            query = Some(q);
        }
        let query = query.unwrap();

        let new_collection = Collection::from_paths(&new_siglist)?.select(&selection)?;
        let index =
            RevIndex::open(output.path(), false, None)?.update(new_collection.try_into()?)?;

        let counter = index.counter_for_query(&query);
        let matches = index.matches_from_counter(counter, 0);

        assert!(matches[0].0.ends_with("/genome-s12.fa.gz"));
        assert_eq!(matches[0].1, 45);

        Ok(())
    }

    #[test]
    fn revindex_load_and_gather() -> Result<()> {
        let mut basedir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        basedir.push("../../tests/test-data/scaled/");

        let siglist: Vec<_> = (10..=12)
            .map(|i| {
                let mut filename = basedir.clone();
                filename.push(format!("genome-s{}.fa.gz.sig", i));
                filename
            })
            .collect();

        let selection = Selection::builder().ksize(31).scaled(10000).build();
        let output = TempDir::new()?;

        let mut query = None;
        let query_sig = Signature::from_path(&siglist[0])?
            .swap_remove(0)
            .select(&selection)?;
        if let Some(q) = prepare_query(query_sig, &selection) {
            query = Some(q);
        }
        let query = query.unwrap();

        {
            let collection = Collection::from_paths(&siglist)?.select(&selection)?;
            let _index = RevIndex::create(output.path(), collection.try_into()?, false);
        }

        let index = RevIndex::open(output.path(), true, None)?;

        let (counter, query_colors, hash_to_color) = index.prepare_gather_counters(&query);

        let matches = index.gather(
            counter,
            query_colors,
            hash_to_color,
            0,
            &query,
            Some(selection),
        )?;

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name(), ""); // signature name is empty
        assert_eq!(matches[0].f_match(), 1.0);

        Ok(())
    }

    #[test]
    fn revindex_load_and_gather_2() -> Result<()> {
        let mut basedir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        basedir.push("../../tests/test-data/gather/");

        let against = vec![
            "GCF_000006945.2_ASM694v2_genomic.fna.gz.sig",
            "GCF_000007545.1_ASM754v1_genomic.fna.gz.sig",
            "GCF_000008105.1_ASM810v1_genomic.fna.gz.sig",
            "GCF_000008545.1_ASM854v1_genomic.fna.gz.sig",
            "GCF_000009085.1_ASM908v1_genomic.fna.gz.sig",
            "GCF_000009505.1_ASM950v1_genomic.fna.gz.sig",
            "GCF_000009525.1_ASM952v1_genomic.fna.gz.sig",
            "GCF_000011885.1_ASM1188v1_genomic.fna.gz.sig",
            "GCF_000016045.1_ASM1604v1_genomic.fna.gz.sig",
            "GCF_000016785.1_ASM1678v1_genomic.fna.gz.sig",
            "GCF_000018945.1_ASM1894v1_genomic.fna.gz.sig",
            "GCF_000195995.1_ASM19599v1_genomic.fna.gz.sig",
        ];
        let against: Vec<_> = against
            .iter()
            .map(|sig| {
                let mut filename = basedir.clone();
                filename.push(sig);
                filename
            })
            .collect();

        // build 'against' sketches into a revindex
        let selection = Selection::builder().ksize(21).scaled(10000).build();
        let output = TempDir::new()?;

        let collection = Collection::from_paths(&against)?.select(&selection)?;
        let _index = RevIndex::create(output.path(), collection.try_into()?, false);

        let index = RevIndex::open(output.path(), true, None)?;

        let mut query = None;
        let mut query_filename = basedir.clone();
        query_filename.push("combined.sig");
        let query_sig = Signature::from_path(query_filename)?
            .swap_remove(0)
            .select(&selection)?;

        if let Some(q) = prepare_query(query_sig, &selection) {
            query = Some(q);
        }
        let query = query.unwrap();

        let (counter, query_colors, hash_to_color) = index.prepare_gather_counters(&query);

        let matches = index.gather(
            counter,
            query_colors,
            hash_to_color,
            5, // 50kb threshold
            &query,
            Some(selection),
        )?;

        // should be 11, based on test_gather_metagenome_num_results
        assert_eq!(matches.len(), 11);

        fn round5(a: f64) -> f64 {
            (a * 1e5).round() / 1e5
        }

        let match_ = &matches[0];
        let names: Vec<&str> = match_.name().split(' ').take(1).collect();
        assert_eq!(names[0], "NC_003198.1");
        assert_eq!(match_.f_match(), 1.0);
        assert_eq!(round5(match_.f_unique_to_query()), round5(0.33219645));

        let match_ = &matches[1];
        let names: Vec<&str> = match_.name().split(' ').take(1).collect();
        assert_eq!(names[0], "NC_000853.1");
        assert_eq!(match_.f_match(), 1.0);
        assert_eq!(round5(match_.f_unique_to_query()), round5(0.13096862));

        let match_ = &matches[2];
        let names: Vec<&str> = match_.name().split(' ').take(1).collect();
        assert_eq!(names[0], "NC_011978.1");
        assert_eq!(match_.f_match(), 0.898936170212766);
        assert_eq!(round5(match_.f_unique_to_query()), round5(0.115279));

        let match_ = &matches[3];
        let names: Vec<&str> = match_.name().split(' ').take(1).collect();
        assert_eq!(names[0], "NC_002163.1");
        assert_eq!(match_.f_match(), 1.0);
        assert_eq!(round5(match_.f_unique_to_query()), round5(0.10709413));

        let match_ = &matches[4];
        let names: Vec<&str> = match_.name().split(' ').take(1).collect();
        assert_eq!(names[0], "NC_003197.2");
        assert_eq!(round5(match_.f_match()), round5(0.31340206));
        assert_eq!(round5(match_.f_unique_to_query()), round5(0.103683));

        let match_ = &matches[5];
        dbg!(match_);
        let names: Vec<&str> = match_.name().split(' ').take(1).collect();
        assert_eq!(names[0], "NC_009486.1");
        assert_eq!(round5(match_.f_match()), round5(0.4842105));
        assert_eq!(round5(match_.f_unique_to_query()), round5(0.0627557));

        let match_ = &matches[6];
        dbg!(match_);
        let names: Vec<&str> = match_.name().split(' ').take(1).collect();
        assert_eq!(names[0], "NC_006905.1");
        assert_eq!(round5(match_.f_match()), round5(0.161016949152542));
        assert_eq!(
            round5(match_.f_unique_to_query()),
            round5(0.0518417462482947)
        );

        let match_ = &matches[7];
        dbg!(match_);
        let names: Vec<&str> = match_.name().split(' ').take(1).collect();
        assert_eq!(names[0], "NC_011080.1");
        assert_eq!(round5(match_.f_match()), round5(0.125799573560768));
        assert_eq!(
            round5(match_.f_unique_to_query()),
            round5(0.04024556616643930)
        );

        let match_ = &matches[8];
        dbg!(match_);
        let names: Vec<&str> = match_.name().split(' ').take(1).collect();
        assert_eq!(names[0], "NC_011274.1");
        assert_eq!(round5(match_.f_match()), round5(0.0919037199124727));
        assert_eq!(
            round5(match_.f_unique_to_query()),
            round5(0.0286493860845839)
        );

        let match_ = &matches[9];
        dbg!(match_);
        let names: Vec<&str> = match_.name().split(' ').take(1).collect();
        assert_eq!(names[0], "NC_006511.1");
        assert_eq!(round5(match_.f_match()), round5(0.0725995316159251));
        assert_eq!(
            round5(match_.f_unique_to_query()),
            round5(0.021145975443383400)
        );

        let match_ = &matches[10];
        dbg!(match_);
        let names: Vec<&str> = match_.name().split(' ').take(1).collect();
        assert_eq!(names[0], "NC_011294.1");
        assert_eq!(round5(match_.f_match()), round5(0.0148619957537155));
        assert_eq!(
            round5(match_.f_unique_to_query()),
            round5(0.0047748976807639800)
        );

        Ok(())
    }

    #[test]
    // a more detailed/focused version of revindex_load_and_gather_2,
    // added in sourmash#3193 for debugging purposes.
    fn revindex_load_and_gather_3() -> Result<()> {
        let mut basedir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        basedir.push("../../tests/test-data/gather/");

        let against = vec![
            "GCF_000016785.1_ASM1678v1_genomic.fna.gz.sig",
            "GCF_000018945.1_ASM1894v1_genomic.fna.gz.sig",
            "GCF_000008545.1_ASM854v1_genomic.fna.gz.sig",
        ];
        let against: Vec<_> = against
            .iter()
            .map(|sig| {
                let mut filename = basedir.clone();
                filename.push(sig);
                filename
            })
            .collect();

        // build 'against' sketches into a revindex
        let selection = Selection::builder().ksize(21).scaled(10000).build();
        let output = TempDir::new()?;

        let collection = Collection::from_paths(&against)?.select(&selection)?;
        let _index = RevIndex::create(output.path(), collection.try_into()?, false);

        let index = RevIndex::open(output.path(), true, None)?;

        let mut query = None;
        let mut query_filename = basedir.clone();
        query_filename.push("combined.sig");
        let query_sig = Signature::from_path(query_filename)?
            .swap_remove(0)
            .select(&selection)?;

        if let Some(q) = prepare_query(query_sig, &selection) {
            query = Some(q);
        }
        let query = query.unwrap();

        let (counter, query_colors, hash_to_color) = index.prepare_gather_counters(&query);

        let matches = index.gather(
            counter,
            query_colors,
            hash_to_color,
            0,
            &query,
            Some(selection),
        )?;

        // should be 3.
        // see sourmash#3193.
        assert_eq!(matches.len(), 3);

        fn round5(a: f64) -> f64 {
            (a * 1e5).round() / 1e5
        }

        let match_ = &matches[0];
        let names: Vec<&str> = match_.name().split(' ').take(1).collect();
        assert_eq!(names[0], "NC_000853.1");
        assert_eq!(match_.f_match(), 1.0);
        assert_eq!(round5(match_.f_unique_to_query()), round5(0.13096862));
        assert_eq!(match_.unique_intersect_bp, 1920000);
        assert_eq!(match_.remaining_bp, 12740000);
        assert_eq!(round5(match_.query_containment_ani()), round5(0.90773763));

        let match_ = &matches[1];
        let names: Vec<&str> = match_.name().split(' ').take(1).collect();
        assert_eq!(names[0], "NC_011978.1");
        assert_eq!(match_.f_match(), 0.898936170212766);
        assert_eq!(round5(match_.f_unique_to_query()), round5(0.115279));
        assert_eq!(match_.unique_intersect_bp, 1690000);
        assert_eq!(match_.remaining_bp, 11050000);
        assert_eq!(round5(match_.query_containment_ani()), round5(0.9068280));

        let match_ = &matches[2];
        dbg!(match_);
        let names: Vec<&str> = match_.name().split(' ').take(1).collect();
        assert_eq!(names[0], "NC_009486.1");
        assert_eq!(round5(match_.f_match()), round5(0.4842105));
        assert_eq!(round5(match_.f_unique_to_query()), round5(0.0627557));
        assert_eq!(match_.unique_intersect_bp, 920000);
        assert_eq!(match_.remaining_bp, 10130000);
        assert_eq!(round5(match_.query_containment_ani()), round5(0.90728512));

        Ok(())
    }

    #[test]
    fn revindex_move() -> Result<()> {
        let basedir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

        let mut zip_collection = basedir.clone();
        zip_collection.push("../../tests/test-data/track_abund/track_abund.zip");

        let outdir = TempDir::new()?;

        let zip_copy = PathBuf::from(
            outdir
                .path()
                .join("sigs.zip")
                .into_os_string()
                .into_string()
                .unwrap(),
        );
        std::fs::copy(zip_collection, zip_copy.as_path())?;

        let selection = Selection::builder().ksize(31).scaled(10000).build();
        let collection = Collection::from_zipfile(zip_copy.as_path())?.select(&selection)?;
        let output = outdir.path().join("index");

        let query = prepare_query(collection.sig_for_dataset(0)?.into(), &selection).unwrap();

        {
            RevIndex::create(output.as_path(), collection.try_into()?, false)?;
        }

        {
            let index = RevIndex::open(output.as_path(), false, None)?;

            let counter = index.counter_for_query(&query);
            let matches = index.matches_from_counter(counter, 0);

            assert!(matches[0].0.starts_with("NC_009665.1"));
            assert_eq!(matches[0].1, 514);
        }

        let new_zip = outdir
            .path()
            .join("new_sigs.zip")
            .into_os_string()
            .into_string()
            .unwrap();
        std::fs::rename(zip_copy, &new_zip)?;

        // RevIndex can't know where the new sigs are
        assert!(RevIndex::open(output.as_path(), false, None).is_err());

        let index = RevIndex::open(output.as_path(), false, Some(&format!("zip://{}", new_zip)))?;

        let counter = index.counter_for_query(&query);
        let matches = index.matches_from_counter(counter, 0);

        assert!(matches[0].0.starts_with("NC_009665.1"));
        assert_eq!(matches[0].1, 514);

        Ok(())
    }

    #[test]
    fn revindex_internalize_storage() -> Result<()> {
        let basedir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

        let mut zip_collection = basedir.clone();
        zip_collection.push("../../tests/test-data/track_abund/track_abund.zip");

        let outdir = TempDir::new()?;

        let zip_copy = PathBuf::from(
            outdir
                .path()
                .join("sigs.zip")
                .into_os_string()
                .into_string()
                .unwrap(),
        );
        std::fs::copy(zip_collection, zip_copy.as_path())?;

        let selection = Selection::builder().ksize(31).scaled(10000).build();
        let collection = Collection::from_zipfile(zip_copy.as_path())?.select(&selection)?;
        let output = outdir.path().join("index");

        let query = prepare_query(collection.sig_for_dataset(0)?.into(), &selection).unwrap();

        let index = RevIndex::create(output.as_path(), collection.try_into()?, false)?;

        let (counter, query_colors, hash_to_color) = index.prepare_gather_counters(&query);

        let matches_external = index
            .gather(
                counter,
                query_colors,
                hash_to_color,
                0,
                &query,
                Some(selection.clone()),
            )
            .expect("failed to gather!");

        {
            let mut index = index;
            index
                .internalize_storage()
                .expect("Error internalizing storage");

            let (counter, query_colors, hash_to_color) = index.prepare_gather_counters(&query);

            let matches_internal = index.gather(
                counter,
                query_colors,
                hash_to_color,
                0,
                &query,
                Some(selection.clone()),
            )?;
            assert_eq!(matches_external, matches_internal);
        }
        let new_path = outdir.path().join("new_index_path");
        std::fs::rename(output.as_path(), new_path.as_path())?;

        let index = RevIndex::open(new_path, false, None)?;

        let (counter, query_colors, hash_to_color) = index.prepare_gather_counters(&query);

        let matches_moved = index.gather(
            counter,
            query_colors,
            hash_to_color,
            0,
            &query,
            Some(selection.clone()),
        )?;
        assert_eq!(matches_external, matches_moved);

        Ok(())
    }

    #[test]
    fn rocksdb_storage_from_path() -> Result<()> {
        let basedir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

        let mut zip_collection = basedir.clone();
        zip_collection.push("../../tests/test-data/track_abund/track_abund.zip");

        let outdir = TempDir::new()?;

        let zip_copy = PathBuf::from(
            outdir
                .path()
                .join("sigs.zip")
                .into_os_string()
                .into_string()
                .unwrap(),
        );
        std::fs::copy(zip_collection, zip_copy.as_path())?;

        let selection = Selection::builder().ksize(31).scaled(10000).build();
        let collection = Collection::from_zipfile(zip_copy.as_path())?.select(&selection)?;
        let output = outdir.path().join("index");

        // Step 1: create an index
        let index = RevIndex::create(output.as_path(), collection.try_into()?, false)?;

        // Step 2: internalize the storage for the index
        {
            let mut index = index;
            index
                .internalize_storage()
                .expect("Error internalizing storage");
        }

        // Step 3: load rocksdb storage from path
        // should have the same content as zipfile

        // Iter thru collection, make sure all records are present
        let collection = Collection::from_zipfile(zip_copy.as_path())?.select(&selection)?;
        assert_eq!(collection.len(), 2);
        let col_storage = collection.storage();

        let spec;
        {
            let rdb_storage = RocksDBStorage::from_path(output.as_os_str().to_str().unwrap());
            spec = rdb_storage.spec();
            collection.iter().for_each(|(_, r)| {
                assert_eq!(
                    rdb_storage.load(r.internal_location().as_str()).unwrap(),
                    col_storage.load(r.internal_location().as_str()).unwrap()
                );
            });
        }

        // Step 4: verify rocksdb storage spec
        assert_eq!(
            spec,
            format!("rocksdb://{}", output.as_os_str().to_str().unwrap())
        );

        let storage = InnerStorage::from_spec(spec)?;
        collection.iter().for_each(|(_, r)| {
            assert_eq!(
                storage.load(r.internal_location().as_str()).unwrap(),
                col_storage.load(r.internal_location().as_str()).unwrap()
            );
        });

        Ok(())
    }

    #[test]
    fn rocksdb_storage_fail_bad_directory() -> Result<()> {
        let testdir = TempDir::new()?;

        match RevIndex::open(testdir, true, None) {
            Err(_) => Ok(()),
            Ok(_) => panic!("test should not reach here"),
        }
    }
}
