
use std::str;
use std::collections::HashMap;
use std::iter::Peekable;
use std::mem::transmute;
use std::collections::VecDeque;
use std::iter::Iterator;

use key_builder::{KeyBuilder, Segment};
use json_value::{JsonValue};
use query::{AggregateFun, SortInfo};

use rocksdb::{self, DBIterator, IteratorMode};

#[derive(Clone)]
pub enum PathSegment {
    ObjectKey(String),
    Array(u64),
    ArrayAll,
}

#[derive(Clone)]
pub struct ReturnPath {
    path: Vec<PathSegment>,
}

impl ReturnPath {
    pub fn new() -> ReturnPath {
        ReturnPath{path: Vec::new()}
    }

    pub fn push_object_key(&mut self, key: String) {
        self.path.push(PathSegment::ObjectKey(key));
    }

    pub fn push_array(&mut self, index: u64) {
        self.path.push(PathSegment::Array(index));
    }

    pub fn push_array_all(&mut self) {
        self.path.push(PathSegment::ArrayAll);
    }

    pub fn to_key(&self) -> String {
        let mut key = String::new();
        for seg in self.path.iter() {
            match seg {
                &PathSegment::ObjectKey(ref str) => {
                    key.push('.');
                    for cc in str.chars() {
                        // Escape chars that conflict with delimiters
                        if "\\$.".contains(cc) {
                            key.push('\\');
                        }
                        key.push(cc);
                    }
                },
                &PathSegment::Array(ref i) => {
                    key.push('$');
                    key.push_str(&i.to_string());
                },
                &PathSegment::ArrayAll => {
                    key.push_str("$*");
                },
            }
        }
        key
    }

    fn nth(&self, i: usize) -> Option<&PathSegment> {
        if self.path.len() <= i {
            None
        } else {
            Some(&self.path[i])
        }
    }
}



/// Returnables are created from parsing the return statement in queries.
/// They nest inside of each other, with the outermost typically being a RetObject or RetArray.
pub trait Returnable {
    /// When a match is found, information about the match is passed to outer most Returnable
    /// and then each nested Returnable will fetch information about the document (fields or
    /// scores or bind variables etc) and convert them to JsonValues and add them to the result
    /// VecDeque.
    fn fetch_result(&self, iter: &mut DBIterator, seq: u64, score: f32,
                    bind_var_keys: &HashMap<String, Vec<String>>,
                    result: &mut VecDeque<JsonValue>);

    /// If aggregates are used each Returnable needs to return information about the
    /// aggregate function it's using and the default value.
    fn get_aggregate_funs(&self, funs: &mut Vec<Option<(AggregateFun, JsonValue)>>);

    /// If a query has a sort clause then we want to match the fields being sorted with
    /// fields being returned. We pass the sorting info by the path of the sorted fields
    /// or scores and Returnables that have the same path will take the sort
    /// information. Any fields not matching a returnable are then added to special hidden
    /// Returnable (RetHidden) which fetches those fields for sorting but not rendered or
    /// returned.
    fn take_sort_for_matching_fields(&mut self, map: &mut HashMap<String, SortInfo>);

    /// Each Returnable will return the sorting direction in the same slot as the returnable
    /// so that later after fetching they will be sorted by QueryResults after fetching but
    /// converting to the final json result.
    fn get_sorting(&mut self, sorts: &mut Vec<Option<SortInfo>>);

    /// This is the final step of a Returnable. The previous fetched JsonValues are now
    /// rendered with other ornamental json elements.
    fn json_result(&self, results: &mut VecDeque<JsonValue>) -> JsonValue;
}

/// A static Json Object the can contain another number of fields and nested returnables.
pub struct RetObject {
    pub fields: Vec<(String, Box<Returnable>)>,
}

impl Returnable for RetObject {
    fn fetch_result(&self, iter: &mut DBIterator, seq: u64, score: f32,
                    bind_var_keys: &HashMap<String, Vec<String>>,
                    result: &mut VecDeque<JsonValue>) {
        for &(ref _key, ref field) in self.fields.iter() {
            field.fetch_result(iter, seq, score, bind_var_keys, result);
        }
    }

    fn get_aggregate_funs(&self, funs: &mut Vec<Option<(AggregateFun, JsonValue)>>) {
        for &(ref _key, ref field) in self.fields.iter() {
            field.get_aggregate_funs(funs);
        }
    }
    
    fn take_sort_for_matching_fields(&mut self, map: &mut HashMap<String, SortInfo>) {
       for &mut (ref _key, ref mut field) in self.fields.iter_mut() {
            field.take_sort_for_matching_fields(map);
       }
    }

    fn get_sorting(&mut self, sorts: &mut Vec<Option<SortInfo>>) {
       for &mut (ref mut _key, ref mut field) in self.fields.iter_mut() {
            field.get_sorting(sorts);
       }
    }

    fn json_result(&self, results: &mut VecDeque<JsonValue>) -> JsonValue {
        let mut vec = Vec::with_capacity(self.fields.len());
        for &(ref key, ref returnable) in self.fields.iter() {
            vec.push((key.clone(), returnable.json_result(results)));
        }
        JsonValue::Object(vec)
    }
}

/// A static Json array the can contain another number of nested Returnables.
pub struct RetArray {
    pub slots: Vec<Box<Returnable>>,
}

impl Returnable for RetArray {
    fn fetch_result(&self, iter: &mut DBIterator, seq: u64, score: f32,
                    bind_var_keys: &HashMap<String, Vec<String>>,
                    result: &mut VecDeque<JsonValue>) {
        for ref slot in self.slots.iter() {
            slot.fetch_result(iter, seq, score, bind_var_keys, result);
        }
    }

    fn get_aggregate_funs(&self, funs: &mut Vec<Option<(AggregateFun, JsonValue)>>) {
       for ref slot in self.slots.iter() {
            slot.get_aggregate_funs(funs);
        }
    }
    
    fn take_sort_for_matching_fields(&mut self, map: &mut HashMap<String, SortInfo>) {
       for slot in self.slots.iter_mut() {
            slot.take_sort_for_matching_fields(map);
       }
    }

    fn get_sorting(&mut self, sorts: &mut Vec<Option<SortInfo>>) {
       for ref mut slot in self.slots.iter_mut() {
            slot.get_sorting(sorts);
       }
    }

    fn json_result(&self, results: &mut VecDeque<JsonValue>) -> JsonValue {
        let mut vec = Vec::with_capacity(self.slots.len());
        for slot in self.slots.iter() {
            vec.push(slot.json_result(results));
        }
        JsonValue::Array(vec)
    }
}

/// A special returnable that only fetches values for later sorting but never renders
/// them back to the caller.
pub struct RetHidden {
    pub unrendered: Vec<Box<Returnable>>,
    pub visible: Box<Returnable>,
}

impl Returnable for RetHidden {
    fn fetch_result(&self, iter: &mut DBIterator, seq: u64, score: f32,
                    bind_var_keys: &HashMap<String, Vec<String>>,
                    result: &mut VecDeque<JsonValue>) {
        for ref unrendered in self.unrendered.iter() {
            unrendered.fetch_result(iter, seq, score, bind_var_keys, result);
        }

        self.visible.fetch_result(iter, seq, score, bind_var_keys, result);
    }

    fn get_aggregate_funs(&self, funs: &mut Vec<Option<(AggregateFun, JsonValue)>>) {
        self.visible.get_aggregate_funs(funs);
    }
    
    fn take_sort_for_matching_fields(&mut self, map: &mut HashMap<String, SortInfo>) {
        self.visible.take_sort_for_matching_fields(map);
    }

    fn get_sorting(&mut self, sorts: &mut Vec<Option<SortInfo>>) {
       for ref mut unrendered in self.unrendered.iter_mut() {
            unrendered.get_sorting(sorts);
        }
       
        self.visible.get_sorting(sorts);
    }

    fn json_result(&self, results: &mut VecDeque<JsonValue>) -> JsonValue {
        for _n in 0..self.unrendered.len() {
            // we already sorted at this point, now discard the values
            results.pop_front();
        }
        self.visible.json_result(results)
    }
}

/// A literal JsonValue. Number, String, Null, True or False. Just in case the query
/// wants to return something that doesn't come from a document.
pub struct RetLiteral {
    pub json: JsonValue,
}

impl Returnable for RetLiteral {
    fn fetch_result(&self, _iter: &mut DBIterator, _seq: u64, _score: f32,
                    _bind_var_keys: &HashMap<String, Vec<String>>,
                    _result: &mut VecDeque<JsonValue>) {
    }

    fn get_aggregate_funs(&self, _funs: &mut Vec<Option<(AggregateFun, JsonValue)>>) {
        //noop
    }
    
    fn take_sort_for_matching_fields(&mut self, _map: &mut HashMap<String, SortInfo>) {
        //noop
    }

    fn get_sorting(&mut self, _sorts: &mut Vec<Option<SortInfo>>) {
        //noop
    }

    fn json_result(&self, _results: &mut VecDeque<JsonValue>) -> JsonValue {
        self.json.clone()
    }
}

/// A value from a document. It knows the path it wants to fetch and loads the value from the
/// stored original document.
pub struct RetValue {
    pub rp: ReturnPath,
    pub ag: Option<(AggregateFun, JsonValue)>,
    pub default: JsonValue,
    pub sort_info: Option<SortInfo>,
}

impl RetValue {
    pub fn bytes_to_json_value(bytes: &[u8]) -> JsonValue {
        match bytes[0] as char {
            's' => {
                let string = unsafe{str::from_utf8_unchecked(&bytes[1..])}.to_string();
                JsonValue::String(string)
            },
            'f' => {
                assert!(bytes.len() == 9);
                let mut bytes2: [u8; 8] = [0; 8];
                for (n, b) in bytes[1..9].iter().enumerate() {
                    bytes2[n] = *b; 
                }
                let double: f64 = unsafe{transmute(bytes2)};
                JsonValue::Number(double)
            },
            'T' => JsonValue::True,
            'F' => JsonValue::False,
            'N' => JsonValue::Null,
            'o' => JsonValue::Object(vec![]),
            'a' => JsonValue::Array(vec![]),
            what => panic!("unexpected type tag in value: {}", what),
        }
    }

    fn return_array(mut array: Vec<(u64, JsonValue)>) -> JsonValue {
        array.sort_by_key(|tuple| tuple.0);
        JsonValue::Array(array.into_iter()
                                 .map(|(_i, json)| json)
                                 .collect())
    }

    fn descend_return_path(iter: &mut DBIterator, seq: u64, kb: &mut KeyBuilder,
            rp: &ReturnPath, mut rp_index: usize) -> Option<JsonValue> {
        
        while let Some(segment) = rp.nth(rp_index) {
            rp_index += 1;
            match segment {
                &PathSegment::ObjectKey(ref string) => {
                    kb.push_object_key(string);
                },
                &PathSegment::ArrayAll => {
                    let mut i = 0;
                    let mut vec = Vec::new();
                    loop {
                        kb.push_array_index(i);
                        i += 1;
                        if let Some(json) = RetValue::descend_return_path(iter, seq,
                                &mut kb.clone(), rp, rp_index) {
                            vec.push(json);
                            kb.pop_array();
                        } else {
                            // we didn't get a value, is it because the array ends or the
                            // full path isn't there? check as there might be more array elements
                            // with a full path that does match.
                            let value_key = kb.value_key(seq);
                            kb.pop_array();

                            // Seek in index to >= entry
                            iter.set_mode(IteratorMode::From(value_key.as_bytes(),
                                                            rocksdb::Direction::Forward));
                            
                            if let Some((key, _value)) = iter.next() {
                                if key.starts_with(value_key.as_bytes()) {
                                    // yes it exists. loop again.
                                    continue; 
                                }
                            }
                            
                            if vec.is_empty() {
                                return None;
                            } else {
                                return Some(JsonValue::Array(vec));
                            }
                        }
                    }
                },
                &PathSegment::Array(ref index) => {
                    kb.push_array_index(*index);
                }
            }
        }

        let value_key = kb.value_key(seq);

        // Seek in index to >= entry
        iter.set_mode(IteratorMode::From(value_key.as_bytes(),
                                        rocksdb::Direction::Forward));
        
        let (key, value) = match iter.next() {
            Some((key, value)) => (key, value),
            None => {
                return None
            },
        };

        if !key.starts_with(value_key.as_bytes()) {
            return None
        }

        let json_value = RetValue::fetch(&mut iter.peekable(), &value_key, key, value);
        Some(json_value)
    }

    fn fetch(iter: &mut Peekable<&mut DBIterator>, value_key: &str,
          mut key: Box<[u8]>, mut value: Box<[u8]>) -> JsonValue {

        if key.len() == value_key.len() {
            // we have a key match!
            return RetValue::bytes_to_json_value(value.as_ref());
        }
        let segment = {
            let key_str = unsafe{str::from_utf8_unchecked(&key)};
            let remaining = &key_str[value_key.len()..];
            KeyBuilder::parse_first_key_value_segment(&remaining)
        };
        
        match segment {
            Some((Segment::ObjectKey(mut unescaped), escaped)) => {
                let mut object: Vec<(String, JsonValue)> = Vec::new();

                let mut value_key_next = value_key.to_string() + &escaped;
                loop {
                    let json_val = RetValue::fetch(iter, &value_key_next, key, value);
                    object.push((unescaped, json_val));
    
                    let segment = match iter.peek() {
                        Some(&(ref k, ref _v)) => {
                            if !k.starts_with(value_key.as_bytes()) {
                                return JsonValue::Object(object);
                            }

                            let key_str = unsafe{str::from_utf8_unchecked(&k)};
                            let remaining = &key_str[value_key.len()..];

                            KeyBuilder::parse_first_key_value_segment(&remaining)
                        },
                        None => return JsonValue::Object(object),
                    };

                    if let Some((Segment::ObjectKey(unescaped2), escaped2)) = segment {
                        unescaped = unescaped2;
                        // advance the peeked iter
                        match iter.next() {
                            Some((k, v)) => {
                                key = k;
                                value = v;
                            }
                            None => panic!("couldn't advanced already peeked iter"),
                        };
                        value_key_next.truncate(value_key.len());
                        value_key_next.push_str(&escaped2);
                    } else {
                        return JsonValue::Object(object);
                    }
                }
            }
            Some((Segment::Array(mut i), escaped)) => {
                // we use a tuple with ordinal because we encounter
                // elements in lexical sorting order instead of ordinal order
                let mut array: Vec<(u64, JsonValue)> = Vec::new();

                let mut value_key_next = value_key.to_string() + &escaped;
                loop {
                    let json_val = RetValue::fetch(iter, &value_key_next, key, value);
                    array.push((i, json_val));
    
                    let segment = match iter.peek() {
                        Some(&(ref k, ref _v)) => {
                            if !k.starts_with(value_key.as_bytes()) {
                                return RetValue::return_array(array);
                            }

                            let key_str = unsafe{str::from_utf8_unchecked(&k)};
                            let remaining = &key_str[value_key.len()..];

                             KeyBuilder::parse_first_key_value_segment(&remaining)
                        },
                        None => return RetValue::return_array(array),
                    };

                    if let Some((Segment::Array(i2), escaped2)) = segment {
                        i = i2;
                        // advance the already peeked iter
                        match iter.next() {
                            Some((k, v)) => {
                                key = k;
                                value = v;
                            },
                            None => panic!("couldn't advanced already peeked iter"),
                        };
                        value_key_next.truncate(value_key.len());
                        value_key_next.push_str(&escaped2);
                    } else {
                        return RetValue::return_array(array);
                    }
                }
            },
            None => {
                let key_str = unsafe{str::from_utf8_unchecked(&key)};
                panic!("somehow couldn't parse key segment {} {}", value_key, key_str);
            },
        }
    }
}

impl Returnable for RetValue {
    fn fetch_result(&self, iter: &mut DBIterator, seq: u64, _score: f32,
                    _bind_var_keys: &HashMap<String, Vec<String>>,
                    result: &mut VecDeque<JsonValue>) {
        if Some((AggregateFun::Count, JsonValue::Null)) == self.ag {
            //don't fetch anything for count(). just stick in a null
            result.push_back(JsonValue::Null);
        }

        let mut kb = KeyBuilder::new();

        if let Some(json) = RetValue::descend_return_path(iter, seq, &mut kb, &self.rp, 0) {
            result.push_back(json);
        } else {
            result.push_back(self.default.clone());
        }
    }

    fn get_aggregate_funs(&self, funs: &mut Vec<Option<(AggregateFun, JsonValue)>>) {
        funs.push(self.ag.clone());
    }
    
    fn take_sort_for_matching_fields(&mut self, map: &mut HashMap<String,SortInfo>) {
        self.sort_info = map.remove(&self.rp.to_key());
    }

    fn get_sorting(&mut self, sorts: &mut Vec<Option<SortInfo>>) {
        sorts.push(self.sort_info.take());
    }

    fn json_result(&self, results: &mut VecDeque<JsonValue>) -> JsonValue {
        if let Some(json) = results.pop_front() {
            json
        } else {
            panic!("missing result!");
        }
    }
}

/// A bind variable. If a bind variable was matched it will be fetched then it's path is
/// added to the bind_var_keys passed into fetch_result(). This will load the values from the
/// original document and return it.
pub struct RetBind {
    pub bind_name: String,
    pub extra_rp: ReturnPath,
    pub ag: Option<(AggregateFun, JsonValue)>,
    pub default: JsonValue,
    pub sort_info: Option<SortInfo>,
}

impl Returnable for RetBind {
    fn fetch_result(&self, iter: &mut DBIterator, seq: u64, _score: f32,
                    bind_var_keys: &HashMap<String, Vec<String>>,
                    result: &mut VecDeque<JsonValue>) {

        if let Some(value_keys) = bind_var_keys.get(&self.bind_name) {
            let mut array = Vec::with_capacity(value_keys.len());
            for base_key in value_keys {
                let mut kb = KeyBuilder::new();

                kb.parse_value_key_path_only(KeyBuilder::value_key_path_only_from_str(&base_key));

                if let Some(json) = RetValue::descend_return_path(iter, seq, &mut kb,
                        &self.extra_rp, 0) {
                    array.push(json);
                } else {
                    array.push(self.default.clone());
                }
            }
            result.push_back(JsonValue::Array(array));
        } else {
            result.push_back(JsonValue::Array(vec![self.default.clone()]));
        }
    }

    fn get_aggregate_funs(&self, funs: &mut Vec<Option<(AggregateFun, JsonValue)>>) {
        funs.push(self.ag.clone());
    }
    
    fn take_sort_for_matching_fields(&mut self, map: &mut HashMap<String,SortInfo>) {
        self.sort_info = map.remove(&(self.bind_name.to_string() + &self.extra_rp.to_key()));
    }

    fn get_sorting(&mut self, sorts: &mut Vec<Option<SortInfo>>) {
        sorts.push(self.sort_info.take());
    }

    fn json_result(&self, results: &mut VecDeque<JsonValue>) -> JsonValue {
        if let Some(json) = results.pop_front() {
            json
        } else {
            panic!("missing bind result!");
        }
    }
}

/// Returns a relevency score for a match.
pub struct RetScore {
    pub sort_info: Option<SortInfo>,
}

impl Returnable for RetScore {
    fn fetch_result(&self, _iter: &mut DBIterator, _seq: u64, score: f32,
                    _bind_var_keys: &HashMap<String, Vec<String>>,
                    result: &mut VecDeque<JsonValue>) {
        result.push_back(JsonValue::Number(score as f64));
    }

    fn get_aggregate_funs(&self, _funs: &mut Vec<Option<(AggregateFun, JsonValue)>>) {
        // noop
    }
    
    fn take_sort_for_matching_fields(&mut self, map: &mut HashMap<String,SortInfo>) {
        self.sort_info = map.remove("score()");
    }

    fn get_sorting(&mut self, sorts: &mut Vec<Option<SortInfo>>) {
        sorts.push(self.sort_info.take());
    }

    fn json_result(&self, results: &mut VecDeque<JsonValue>) -> JsonValue {
        if let Some(json) = results.pop_front() {
            json
        } else {
            panic!("missing score result!");
        }
    }
}
