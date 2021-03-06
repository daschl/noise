extern crate rocksdb;
extern crate rustc_serialize;

use std::collections::HashMap;

use self::rustc_serialize::json::{JsonEvent, Parser, StackElement};
// Needed for a trait in order to `put()` into a `rocksdb::WriteBatch`
use self::rocksdb::Writable;

use key_builder::{KeyBuilder, SegmentType};
use records_capnp::payload;
use stems::Stems;

// Good example of using rustc_serialize: https://github.com/ajroetker/beautician/blob/master/src/lib.rs
// Callback based JSON streaming parser: https://github.com/gyscos/json-streamer.rs
// Another parser pased on rustc_serializ: https://github.com/isagalaev/ijson-rust/blob/master/src/test.rs#L11


#[derive(Debug, PartialEq)]
struct WordInfo {
    //offset in the text field where the stemmed text starts
    stemmed_offset: u64,

    // the suffix of the stemmed text. When applied over stemmed, the original
    // text is returned.
    suffix_text: String,

    // the start of the suffixText
    suffix_offset: u64,
}

type ArrayOffsets = Vec<u64>;
type ArrayOffsetsToWordInfo = HashMap<ArrayOffsets, Vec<WordInfo>>;
type WordPathInfoMap = HashMap<String, ArrayOffsetsToWordInfo>;

#[derive(Debug)]
pub struct Shredder {
    keybuilder: KeyBuilder,
    map: WordPathInfoMap,
    path_array_offsets: ArrayOffsets,
}


impl Shredder {
    pub fn new() -> Shredder {
        Shredder{
            keybuilder: KeyBuilder::new(),
            map: WordPathInfoMap::new(),
            path_array_offsets: Vec::new(),
        }
    }
    fn add_entries(&mut self, text: String, docseq: u64) {
        let stems = Stems::new(text.as_str());
        for stem in stems {
            self.keybuilder.push_word(&stem.stemmed);
            self.keybuilder.push_doc_seq(docseq);
            let map_path_array_offsets = self.map.entry(self.keybuilder.key())
                                                        .or_insert(ArrayOffsetsToWordInfo::new());
            let map_word_infos = map_path_array_offsets.entry(self.path_array_offsets.clone())
                .or_insert(Vec::new());
            map_word_infos.push(WordInfo{
                stemmed_offset: stem.stemmed_offset as u64,
                suffix_text: stem.suffix.to_string(),
                suffix_offset: stem.suffix_offset as u64,
            });
            self.keybuilder.pop_doc_seq();
            self.keybuilder.pop_word();
        }
        println!("add_entries: map: {:?}", self.map);
    }

    fn inc_top_array_offset(&mut self) {
        // we encounter a new element. if we are a child element of an array
        // increment the offset. If we aren't (we are the root value or a map
        // value) we don't increment
        if let Some(SegmentType::Array) = self.keybuilder.last_pushed_segment_type() {
            if let Some(last) = self.path_array_offsets.last_mut() {
                *last += 1;
            }
        }
    }


    pub fn shred(&mut self, json: &str, docseq: u64) -> Result<&str, String> {
        println!("{}", json);
        let mut parser = Parser::new(json.chars());
        let mut token = parser.next();

        loop {
            // Get the next token, so that in case of an `ObjectStart` the key is already
            // on the stack.
            let nexttoken = parser.next();

            match token.take() {
                Some(JsonEvent::ObjectStart) => {
                    match parser.stack().top() {
                        Some(StackElement::Key(key)) => {
                            println!("object start: {:?}", key);
                            self.keybuilder.push_object_key(key.to_string());
                            self.inc_top_array_offset();
                        },
                        _ => {
                            panic!("XXX This is probably an object end");
                        }
                    }
                },
                Some(JsonEvent::ObjectEnd) => {
                    self.keybuilder.pop_object_key();
                },
                Some(JsonEvent::ArrayStart) => {
                    println!("array start");
                    self.keybuilder.push_array();
                    //self.inc_top_array_offset();
                    self.path_array_offsets.push(0);
                },
                Some(JsonEvent::ArrayEnd) => {
                    self.path_array_offsets.pop();
                    self.keybuilder.pop_array();
                },
                Some(JsonEvent::StringValue(value)) => {
                    self.add_entries(value, docseq);
                    self.inc_top_array_offset();
                    //self.keybuilder.pop_object_key();
                },
                not_implemented => {
                    panic!("Not yet implemented other JSON types! {:?}", not_implemented);
                }
            };

            token = nexttoken;
            if token == None {
                break;
            }
        }
        println!("keybuilder: {}", self.keybuilder.key());
        println!("shredder: keys:");
        for key in self.map.keys() {
            println!("  {}", key);
        }
        Ok(&"thedocid")
    }

    pub fn add_to_batch(&self, batch: &rocksdb::WriteBatch) -> Result<(), String> {
        for (key_path, word_path_infos) in &self.map {
            let mut message = ::capnp::message::Builder::new_default();
            {
                let capn_payload = message.init_root::<payload::Builder>();
                let mut capn_arrayoffsets_to_wordinfo = capn_payload.init_arrayoffsets_to_wordinfos(
                    word_path_infos.len() as u32);
                for (infos_pos, (arrayoffsets, wordinfos)) in word_path_infos.iter().enumerate() {

                    let mut capn_a2w = capn_arrayoffsets_to_wordinfo.borrow().get(infos_pos as u32);
                    {
                        let mut capn_arrayoffsets =
                            capn_a2w.borrow().init_arrayoffsets(arrayoffsets.len() as u32);
                        for (pos, arrayoffset) in arrayoffsets.iter().enumerate() {
                            capn_arrayoffsets.set(pos as u32, arrayoffset.clone());
                        }
                    }
                    {
                        let mut capn_wordinfos = capn_a2w.init_wordinfos(wordinfos.len() as u32);
                        for (pos, wordinfo) in wordinfos.iter().enumerate() {
                            let mut capn_wordinfo = capn_wordinfos.borrow().get(pos as u32);
                            capn_wordinfo.set_stemmed_offset(wordinfo.stemmed_offset);
                            capn_wordinfo.set_suffix_text(&wordinfo.suffix_text);
                            capn_wordinfo.set_suffix_offset(wordinfo.suffix_offset);
                        }
                    }
                }
            }
            let mut bytes = Vec::new();
            ::capnp::serialize_packed::write_message(&mut bytes, &message).unwrap();
            try!(batch.put(&key_path.clone().into_bytes(), &bytes));
        }
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::{ArrayOffsetsToWordInfo, WordInfo, WordPathInfoMap};

    #[test]
    fn test_shred_nested() {
        let mut shredder = super::Shredder::new();
        //let json = r#"{"hello": {"my": "world!"}, "anumber": 2}"#;
        //let json = r#"{"A":[{"B":"B2VMX two three","C":"C2"},{"B": "b1","C":"C2"}]}"#;
        //let json = r#"{"A":[[[{"B": "string within deeply nested array should be stemmed"}]]]}"#;
        //let json = r#"[{"A": 1, "B": 2, "C": 3}]"#;
        //let json = r#"{"foo": {"bar": 1}}"#;
        let json = r#"{"some": ["array", "data", ["also", "nested"]]}"#;
        let docseq = 123;
        shredder.shred(json, docseq).unwrap();
        let expected = vec![
            ("W.some$!array#123", vec![
                (vec![0], vec![WordInfo {
                    stemmed_offset: 0, suffix_text: "".to_string(), suffix_offset: 5 }])]),
            ("W.some$!data#123", vec![
                (vec![1], vec![WordInfo {
                    stemmed_offset: 0, suffix_text: "".to_string(), suffix_offset: 4 }])]),
            ("W.some$$!also#123", vec![
                (vec![2, 0], vec![WordInfo {
                    stemmed_offset: 0, suffix_text: "".to_string(), suffix_offset: 4 }])]),
            ("W.some$$!nest#123", vec![
                (vec![2, 1], vec![WordInfo {
                    stemmed_offset: 0, suffix_text: "ed".to_string(), suffix_offset: 4 }])]),
            ];
        compare_shredded(&shredder.map, &expected);
    }

    #[test]
    fn test_shred_objects() {
        let mut shredder = super::Shredder::new();
        let json = r#"{"A":[{"B":"B2VMX two three","C":"..C2"},{"B": "b1","C":"..C2"}]}"#;
        let docseq = 1234;
        shredder.shred(json, docseq).unwrap();
        let expected = vec![
            ("W.A$.B!b1#1234", vec![
                (vec![0], vec![
                    WordInfo {
                        stemmed_offset: 0, suffix_text: "".to_string(), suffix_offset: 2 }])]),
            ("W.A$.B!b2vmx#1234", vec![
                (vec![0], vec![
                    WordInfo {
                        stemmed_offset: 0, suffix_text: "".to_string(), suffix_offset: 5 }])]),
            ("W.A$.B!c2#1234", vec![
                (vec![0], vec![
                    WordInfo {
                        stemmed_offset: 2, suffix_text: "".to_string(), suffix_offset: 4 },
                    WordInfo {
                        stemmed_offset: 2, suffix_text: "".to_string(), suffix_offset: 4 }])]),
            ("W.A$.B!three#1234", vec![
                (vec![0], vec![WordInfo {
                    stemmed_offset: 10, suffix_text: "".to_string(), suffix_offset: 15 }])]),
            ("W.A$.B!two#1234", vec![
                (vec![0], vec![WordInfo {
                    stemmed_offset: 6, suffix_text: "".to_string(), suffix_offset: 9 }])]),
            ];
        compare_shredded(&shredder.map, &expected);
    }

    fn compare_shredded(result_map: &WordPathInfoMap,
                        expected: &Vec<(&str, Vec<(Vec<u64>, Vec<WordInfo>)>)>) {
        // HashMap have an arbitraty order of the elements
        let mut result: Vec<(&String, &ArrayOffsetsToWordInfo)> = result_map.into_iter().collect();
        result.sort_by(|a, b| Ord::cmp(&a.0, &b.0));
        for (ii, &(key, values)) in result.iter().enumerate() {
            assert_eq!(key, expected[ii].0);
            let mut wordinfos: Vec<(&Vec<u64>, &Vec<WordInfo>)> = values.iter().collect();
            wordinfos.sort_by_key(|item| item.0);
            for (jj, wordinfo) in wordinfos.iter().enumerate() {
                assert_eq!(wordinfo.0, &expected[ii].1[jj].0);
                assert_eq!(wordinfo.1, &expected[ii].1[jj].1);
            }
        }
    }
}
