use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use pest::iterators::{Pair, Pairs};
use cozorocks::{SlicePtr};
use crate::db::engine::{Session};
use crate::relation::tuple::{OwnTuple, Tuple};
use crate::relation::typing::Typing;
use crate::relation::value::{METHOD_CONCAT, Value};
use crate::error::{CozoError, Result};
use crate::error::CozoError::LogicError;
use crate::relation::data::DataKind;
use crate::parser::{Rule};
use crate::parser::text_identifier::build_name_in_def;
use crate::relation::value;

/// layouts for sector 0
/// `[Null]`: stores information about table_ids
/// `[Text, Int]`: contains definable data and depth info
/// `[Int, Text]`: inverted index for depth info
/// `[Null, Text, Int, Int, Text]` inverted index for related tables
/// `[Null, Int, Text, Int, Text]` inverted index for related tables
/// `[True, Int]` table info, value is key

impl<'s> Session<'s> {
    pub fn define_variable(&mut self, name: &str, val: &Value, in_root: bool) -> Result<()> {
        let mut data = Tuple::with_data_prefix(DataKind::Value);
        data.push_value(val);
        self.define_data(name, data, in_root)
    }
    fn resolve_value(&self, name: &str) -> Result<Option<Value>> {
        match self.resolve(name)? {
            None => Ok(None),
            Some(t) => {
                match t.data_kind()? {
                    DataKind::Value => Ok(Some(t.get(0)
                        .ok_or_else(|| CozoError::LogicError("Corrupt".to_string()))?
                        .to_static())),
                    k => Err(CozoError::UnexpectedDataKind(k))
                }
            }
        }
    }
    fn encode_definable_key(&self, name: &str, in_root: bool) -> OwnTuple {
        let depth_code = if in_root { 0 } else { self.get_stack_depth() as i64 };
        let mut tuple = Tuple::with_null_prefix();
        tuple.push_str(name);
        tuple.push_int(depth_code);
        tuple
    }
    fn parse_cols(&self, pair: Pair<Rule>) -> Result<(Typing, Typing)> {
        let col_res = pair.into_inner().map(|p| {
            let mut ps = p.into_inner();
            let mut name_ps = ps.next().unwrap().into_inner();
            let is_key;
            let mut name_p = name_ps.next().unwrap();
            match name_p.as_rule() {
                Rule::key_marker => {
                    is_key = true;
                    name_p = name_ps.next().unwrap();
                }
                _ => { is_key = false }
            }
            let name = build_name_in_def(name_p, true)?;
            let type_p = Typing::from_pair(ps.next().unwrap(), Some(self))?;
            Ok((is_key, name, type_p))
        }).collect::<Result<Vec<_>>>()?;
        let all_names = col_res.iter().map(|(_, n, _)| n).collect::<HashSet<_>>();
        if all_names.len() != col_res.len() {
            return Err(CozoError::DuplicateNames(col_res.iter().map(|(_, n, _)| n.to_string()).collect::<Vec<_>>()));
        }
        let (keys, cols): (Vec<_>, Vec<_>) = col_res.iter().partition(|(is_key, _, _)| *is_key);
        let keys_typing = Typing::NamedTuple(keys.iter().map(|(_, n, t)| (n.to_string(), t.clone())).collect());
        let vals_typing = Typing::NamedTuple(cols.iter().map(|(_, n, t)| (n.to_string(), t.clone())).collect());
        Ok((keys_typing, vals_typing))
    }
    fn parse_definition(&self, pair: Pair<Rule>, in_root: bool) -> Result<(bool, (String, OwnTuple, Vec<OwnTuple>))> {
        Ok(match pair.as_rule() {
            Rule::node_def => (true, self.parse_node_def(pair.into_inner(), in_root)?),
            Rule::edge_def => (true, self.parse_edge_def(pair.into_inner(), in_root)?),
            Rule::associate_def => (true, self.parse_assoc_def(pair.into_inner(), in_root)?),
            Rule::index_def => todo!(),
            Rule::type_def => (false, self.parse_type_def(pair.into_inner(), in_root)?),
            _ => unreachable!()
        })
    }
    fn parse_assoc_def(&self, mut pairs: Pairs<Rule>, in_root: bool) -> Result<(String, OwnTuple, Vec<OwnTuple>)> {
        let name = build_name_in_def(pairs.next().unwrap(), true)?;
        let src_name = build_name_in_def(pairs.next().unwrap(), true)?;
        let src_tbl = match self.resolve(&src_name)? {
            Some(res) => res,
            None => return Err(CozoError::UndefinedType(src_name))
        };
        let (_kind, src_global, src_id) = Self::extract_table_id(src_tbl)?;
        if in_root && !src_global {
            return Err(CozoError::LogicError("Cannot have global edge with local nodes".to_string()));
        }

        let (keys_typing, vals_typing) = self.parse_cols(pairs.next().unwrap())?;
        if keys_typing.to_string() != "{}" {
            return Err(CozoError::LogicError("Cannot have keys in assoc".to_string()));
        }

        let mut tuple = Tuple::with_data_prefix(DataKind::Assoc);
        tuple.push_bool(src_global);
        tuple.push_int(src_id);
        tuple.push_str(vals_typing.to_string());

        let mut for_src = Tuple::with_prefix(0);
        for_src.push_null();
        for_src.push_str(&src_name);
        for_src.push_int(if in_root { 0 } else { self.get_stack_depth() as i64 });
        for_src.push_int(DataKind::Assoc as i64);
        for_src.push_str(&name);

        let mut for_src_i = Tuple::with_prefix(0);
        for_src_i.push_null();
        for_src_i.push_int(if in_root { 0 } else { self.get_stack_depth() as i64 });
        for_src_i.push_str(&src_name);
        for_src_i.push_int(DataKind::Assoc as i64);
        for_src_i.push_str(&name);

        Ok((name, tuple, vec![for_src, for_src_i]))
    }
    fn parse_type_def(&self, mut pairs: Pairs<Rule>, _in_root: bool) -> Result<(String, OwnTuple, Vec<OwnTuple>)> {
        let name = build_name_in_def(pairs.next().unwrap(), true)?;
        let typ = Typing::from_pair(pairs.next().unwrap(), Some(self))?;
        let mut data = Tuple::with_data_prefix(DataKind::Type);
        data.push_str(typ.to_string());
        Ok((name, data, vec![]))
    }
    fn parse_edge_def(&self, mut pairs: Pairs<Rule>, in_root: bool) -> Result<(String, OwnTuple, Vec<OwnTuple>)> {
        let src_name = build_name_in_def(pairs.next().unwrap(), true)?;
        let src_tbl = match self.resolve(&src_name)? {
            Some(res) => res,
            None => return Err(CozoError::UndefinedType(src_name))
        };
        let (kind, src_global, src_id) = Self::extract_table_id(src_tbl)?;
        if in_root && !src_global {
            return Err(CozoError::LogicError("Cannot have global edge with local nodes".to_string()));
        }
        if kind != DataKind::Node {
            return Err(CozoError::UnexpectedDataKind(kind));
        }
        let name = build_name_in_def(pairs.next().unwrap(), true)?;
        let dst_name = build_name_in_def(pairs.next().unwrap(), true)?;
        let dst_tbl = match self.resolve(&dst_name)? {
            Some(res) => res,
            None => return Err(CozoError::UndefinedType(dst_name))
        };
        let (kind, dst_global, dst_id) = Self::extract_table_id(dst_tbl)?;
        if in_root && !dst_global {
            return Err(CozoError::LogicError("Cannot have global edge with local nodes".to_string()));
        }
        if kind != DataKind::Node {
            return Err(CozoError::UnexpectedDataKind(kind));
        }
        let (keys_typing, vals_typing) = match pairs.next() {
            Some(p) => self.parse_cols(p)?,
            None => (Typing::NamedTuple(vec![]), Typing::NamedTuple(vec![]))
        };

        let mut tuple = Tuple::with_data_prefix(DataKind::Edge);
        tuple.push_bool(src_global);
        tuple.push_int(src_id);
        tuple.push_bool(dst_global);
        tuple.push_int(dst_id);
        tuple.push_str(keys_typing.to_string());
        tuple.push_str(vals_typing.to_string());
        tuple.push_null(); // TODO default values for keys
        tuple.push_null(); // TODO default values for cols

        let mut index_data = Vec::with_capacity(2);

        let mut for_src = Tuple::with_prefix(0);
        for_src.push_null();
        for_src.push_str(&src_name);
        for_src.push_int(if in_root { 0 } else { self.get_stack_depth() as i64 });
        for_src.push_int(DataKind::Edge as i64);
        for_src.push_str(&name);

        index_data.push(for_src);

        let mut for_src_i = Tuple::with_prefix(0);
        for_src_i.push_null();
        for_src_i.push_int(if in_root { 0 } else { self.get_stack_depth() as i64 });
        for_src_i.push_str(&src_name);
        for_src_i.push_int(DataKind::Edge as i64);
        for_src_i.push_str(&name);

        index_data.push(for_src_i);

        if dst_name != src_name {
            let mut for_dst = Tuple::with_prefix(0);
            for_dst.push_null();
            for_dst.push_str(&dst_name);
            for_dst.push_int(if in_root { 0 } else { self.get_stack_depth() as i64 });
            for_dst.push_int(DataKind::Edge as i64);
            for_dst.push_str(&name);

            index_data.push(for_dst);

            let mut for_dst_i = Tuple::with_prefix(0);
            for_dst_i.push_null();
            for_dst_i.push_int(if in_root { 0 } else { self.get_stack_depth() as i64 });
            for_dst_i.push_str(&dst_name);
            for_dst_i.push_int(DataKind::Edge as i64);
            for_dst_i.push_str(&name);

            index_data.push(for_dst_i);
        }

        Ok((name, tuple, index_data))
    }

    fn extract_table_id<T: AsRef<[u8]>>(src_tbl: Tuple<T>) -> Result<(DataKind, bool, i64)> {
        let kind = src_tbl.data_kind()?;
        match kind {
            DataKind::Data | DataKind::Value | DataKind::Type => return Err(CozoError::UnexpectedDataKind(kind)),
            _ => {}
        };
        let is_global = src_tbl.get_bool(0).expect("Data corrupt");
        let table_id = src_tbl.get_int(1).expect("Data corrupt");
        Ok((kind, is_global, table_id))
    }
    fn parse_node_def(&self, mut pairs: Pairs<Rule>, _in_root: bool) -> Result<(String, OwnTuple, Vec<OwnTuple>)> {
        let name = build_name_in_def(pairs.next().unwrap(), true)?;
        let col_pair = pairs.next().unwrap();
        let (keys_typing, vals_typing) = self.parse_cols(col_pair)?;
        let mut tuple = Tuple::with_data_prefix(DataKind::Node);
        tuple.push_str(keys_typing.to_string());
        tuple.push_str(vals_typing.to_string());
        tuple.push_null(); // TODO default values for keys
        tuple.push_null(); // TODO default values for cols
        Ok((name, tuple, vec![]))
    }
    pub fn run_definition(&mut self, pair: Pair<Rule>) -> Result<()> {
        let in_root = match pair.as_rule() {
            Rule::global_def => true,
            Rule::local_def => false,
            r => panic!("Encountered definition with rule {:?}", r)
        };

        let (need_id, (name, mut tuple, assoc_defs)) = self.parse_definition(
            pair.into_inner().next().unwrap(), in_root,
        )?;
        if need_id {
            let id = self.get_next_storage_id(in_root)?;
            tuple = tuple.insert_values_at(0, &[in_root.into(),
                id.into()]);
            let mut id_key = Tuple::with_null_prefix();
            id_key.push_bool(true);
            id_key.push_int(id);
            self.define_raw_key(&id_key, Some(&tuple), in_root).unwrap();
        }
        for t in assoc_defs {
            self.define_raw_key(&t, None, in_root).unwrap();
        }
        self.define_data(&name, tuple, in_root)
    }
    pub fn partial_eval<'a>(&self, value: Value<'a>, params: &BTreeMap<String, Value<'a>>,
                            table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        match value {
            v @ (Value::Null |
            Value::Bool(_) |
            Value::Int(_) |
            Value::Float(_) |
            Value::Uuid(_) |
            Value::Text(_) |
            Value::EndSentinel) => Ok((true, v)),
            Value::List(l) => {
                let init_vec = Vec::with_capacity(l.len());
                let res: Result<(bool, Vec<Value>)> = l.into_iter()
                    .try_fold((true, init_vec), |(is_evaluated, mut accum), val| {
                        let (ev, new_val) = self.partial_eval(val, params, table_bindings)?;
                        accum.push(new_val);
                        Ok((ev && is_evaluated, accum))
                    });
                let (is_ev, v) = res?;
                Ok((is_ev, v.into()))
            }
            Value::Dict(d) => {
                let res: Result<(bool, BTreeMap<Cow<str>, Value>)> = d.into_iter()
                    .try_fold((true, BTreeMap::new()), |(is_evaluated, mut accum), (k, v)| {
                        let (ev, new_val) = self.partial_eval(v, params, table_bindings)?;
                        accum.insert(k, new_val);
                        Ok((ev && is_evaluated, accum))
                    });
                let (is_ev, v) = res?;
                Ok((is_ev, v.into()))
            }
            Value::Variable(v) => {
                if v.starts_with('$') {
                    Ok(if let Some(d) = params.get(v.as_ref()) {
                        (true, d.clone())
                    } else {
                        (false, Value::Variable(v))
                    })
                } else {
                    Ok(match self.resolve_value(&v)? {
                        None => (false, Value::Variable(v)),
                        Some(rs) => {
                            (rs.is_evaluated(), rs.to_static())
                        }
                    })
                }
            }
            Value::FieldAccess(field, arg) => {
                match *arg {
                    v @ (Value::Variable(_) |
                    Value::IdxAccess(_, _) |
                    Value::FieldAccess(_, _) |
                    Value::Apply(_, _)) => Ok((false, Value::FieldAccess(field, v.into()))),
                    Value::Dict(mut d) => {
                        Ok(d.remove(field.as_ref())
                            .map(|v| (v.is_evaluated(), v))
                            .unwrap_or((true, Value::Null)))
                    }
                    _ => Err(LogicError("Field access failed".to_string()))
                }
            }
            Value::IdxAccess(idx, arg) => {
                match *arg {
                    v @ (Value::Variable(_) |
                    Value::IdxAccess(_, _) |
                    Value::FieldAccess(_, _) |
                    Value::Apply(_, _)) => Ok((false, Value::IdxAccess(idx, v.into()))),
                    Value::List(mut l) => {
                        if idx >= l.len() {
                            Ok((true, Value::Null))
                        } else {
                            let v = l.swap_remove(idx);
                            Ok((v.is_evaluated(), v))
                        }
                    }
                    _ => Err(LogicError("Idx access failed".to_string()))
                }
            }
            Value::Apply(op, args) => {
                Ok(match op.as_ref() {
                    value::OP_STR_CAT => self.str_cat_values(args, params, table_bindings)?,
                    value::OP_ADD => self.add_values(args, params, table_bindings)?,
                    value::OP_SUB => self.sub_values(args, params, table_bindings)?,
                    value::OP_MUL => self.mul_values(args, params, table_bindings)?,
                    value::OP_DIV => self.div_values(args, params, table_bindings)?,
                    value::OP_EQ => self.eq_values(args, params, table_bindings)?,
                    value::OP_NE => self.ne_values(args, params, table_bindings)?,
                    value::OP_OR => self.or_values(args, params, table_bindings)?,
                    value::OP_AND => self.and_values(args, params, table_bindings)?,
                    value::OP_MOD => self.mod_values(args, params, table_bindings)?,
                    value::OP_GT => self.gt_values(args, params, table_bindings)?,
                    value::OP_GE => self.ge_values(args, params, table_bindings)?,
                    value::OP_LT => self.lt_values(args, params, table_bindings)?,
                    value::OP_LE => self.le_values(args, params, table_bindings)?,
                    value::OP_POW => self.pow_values(args, params, table_bindings)?,
                    value::OP_COALESCE => self.coalesce_values(args, params, table_bindings)?,
                    value::OP_NEGATE => self.negate_values(args, params, table_bindings)?,
                    value::OP_MINUS => self.minus_values(args, params, table_bindings)?,
                    value::METHOD_IS_NULL => self.is_null_values(args, params, table_bindings)?,
                    value::METHOD_NOT_NULL => self.not_null_values(args, params, table_bindings)?,
                    value::METHOD_CONCAT => self.concat_values(args, params, table_bindings)?,
                    _ => { todo!() }
                })
            }
        }
    }

    fn coalesce_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                           table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let res = args.into_iter().try_fold(vec![], |mut accum, cur| {
            match self.partial_eval(cur, params, table_bindings) {
                Ok((ev, cur)) => {
                    if ev {
                        if cur == Value::Null {
                            Ok(accum)
                        } else {
                            Err(Ok(cur))
                        }
                    } else {
                        accum.push(cur);
                        Ok(accum)
                    }
                }
                Err(e) => Err(Err(e))
            }
        });
        match res {
            Ok(accum) => {
                match accum.len() {
                    0 => Ok((true, Value::Null)),
                    1 => Ok((false, accum.into_iter().next().unwrap())),
                    _ => Ok((false, Value::Apply(value::OP_COALESCE.into(), accum)))
                }
            }
            Err(Ok(v)) => Ok((true, v)),
            Err(Err(e)) => Err(e)
        }
    }
    fn str_cat_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                          table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        let (re, right) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null || right == Value::Null {
            return Ok((true, Value::Null));
        }
        if !le || !re {
            return Ok((false, Value::Apply(value::OP_STR_CAT.into(), vec![left, right])));
        }
        Ok(match (left, right) {
            (Value::Text(l), Value::Text(r)) => (true, (l.to_string() + r.as_ref()).into()),
            (_, _) => return Err(CozoError::InvalidArgument)
        })
    }
    fn add_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                      table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        let (re, right) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null || right == Value::Null {
            return Ok((true, Value::Null));
        }
        if !le || !re {
            return Ok((false, Value::Apply(value::OP_ADD.into(), vec![left, right])));
        }
        Ok(match (left, right) {
            (Value::Int(l), Value::Int(r)) => (true, (l + r).into()),
            (Value::Float(l), Value::Int(r)) => (true, (l + (r as f64)).into()),
            (Value::Int(l), Value::Float(r)) => (true, ((l as f64) + r.into_inner()).into()),
            (Value::Float(l), Value::Float(r)) => (true, (l.into_inner() + r.into_inner()).into()),
            (_, _) => return Err(CozoError::InvalidArgument)
        })
    }
    fn sub_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                      table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        let (re, right) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null || right == Value::Null {
            return Ok((true, Value::Null));
        }
        if !le || !re {
            return Ok((false, Value::Apply(value::OP_SUB.into(), vec![left, right])));
        }
        Ok(match (left, right) {
            (Value::Int(l), Value::Int(r)) => (true, (l - r).into()),
            (Value::Float(l), Value::Int(r)) => (true, (l - (r as f64)).into()),
            (Value::Int(l), Value::Float(r)) => (true, ((l as f64) - r.into_inner()).into()),
            (Value::Float(l), Value::Float(r)) => (true, (l.into_inner() - r.into_inner()).into()),
            (_, _) => return Err(CozoError::InvalidArgument)
        })
    }
    fn minus_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                        table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null {
            return Ok((true, Value::Null));
        }
        if !le {
            return Ok((false, Value::Apply(value::OP_MINUS.into(), vec![left])));
        }
        Ok(match left {
            Value::Int(l) => (true, (-l).into()),
            Value::Float(l) => (true, (-l).into()),
            _ => return Err(CozoError::InvalidArgument)
        })
    }
    fn negate_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                         table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null {
            return Ok((true, Value::Null));
        }
        if !le {
            return Ok((false, Value::Apply(value::OP_NEGATE.into(), vec![left])));
        }
        Ok(match left {
            Value::Bool(l) => (true, (!l).into()),
            _ => return Err(CozoError::InvalidArgument)
        })
    }
    fn is_null_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                          table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null {
            return Ok((true, true.into()));
        }
        if !le {
            return Ok((false, Value::Apply(value::METHOD_IS_NULL.into(), vec![left])));
        }
        Ok((true, false.into()))
    }
    fn not_null_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                           table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null {
            return Ok((true, false.into()));
        }
        if !le {
            return Ok((false, Value::Apply(value::METHOD_NOT_NULL.into(), vec![left])));
        }
        Ok((true, true.into()))
    }
    fn pow_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                      table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        let (re, right) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null || right == Value::Null {
            return Ok((true, Value::Null));
        }
        if !le || !re {
            return Ok((false, Value::Apply(value::OP_POW.into(), vec![left, right])));
        }
        Ok(match (left, right) {
            (Value::Int(l), Value::Int(r)) => (true, ((l as f64).powf(r as f64)).into()),
            (Value::Float(l), Value::Int(r)) => (true, ((l.into_inner()).powf(r as f64)).into()),
            (Value::Int(l), Value::Float(r)) => (true, ((l as f64).powf(r.into_inner())).into()),
            (Value::Float(l), Value::Float(r)) => (true, ((l.into_inner()).powf(r.into_inner())).into()),
            (_, _) => return Err(CozoError::InvalidArgument)
        })
    }
    fn gt_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                     table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        let (re, right) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null || right == Value::Null {
            return Ok((true, Value::Null));
        }
        if !le || !re {
            return Ok((false, Value::Apply(value::OP_GT.into(), vec![left, right])));
        }
        Ok(match (left, right) {
            (Value::Int(l), Value::Int(r)) => (true, (l > r).into()),
            (Value::Float(l), Value::Int(r)) => (true, (l > (r as f64).into()).into()),
            (Value::Int(l), Value::Float(r)) => (true, ((l as f64) > r.into_inner()).into()),
            (Value::Float(l), Value::Float(r)) => (true, (l > r).into()),
            (_, _) => return Err(CozoError::InvalidArgument)
        })
    }
    fn lt_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                     table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        let (re, right) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null || right == Value::Null {
            return Ok((true, Value::Null));
        }
        if !le || !re {
            return Ok((false, Value::Apply(value::OP_LT.into(), vec![left, right])));
        }
        Ok(match (left, right) {
            (Value::Int(l), Value::Int(r)) => (true, (l < r).into()),
            (Value::Float(l), Value::Int(r)) => (true, (l < (r as f64).into()).into()),
            (Value::Int(l), Value::Float(r)) => (true, ((l as f64) < r.into_inner()).into()),
            (Value::Float(l), Value::Float(r)) => (true, (l < r).into()),
            (_, _) => return Err(CozoError::InvalidArgument)
        })
    }
    fn ge_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                     table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        let (re, right) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null || right == Value::Null {
            return Ok((true, Value::Null));
        }
        if !le || !re {
            return Ok((false, Value::Apply(value::OP_GE.into(), vec![left, right])));
        }
        Ok(match (left, right) {
            (Value::Int(l), Value::Int(r)) => (true, (l >= r).into()),
            (Value::Float(l), Value::Int(r)) => (true, (l >= (r as f64).into()).into()),
            (Value::Int(l), Value::Float(r)) => (true, ((l as f64) >= r.into_inner()).into()),
            (Value::Float(l), Value::Float(r)) => (true, (l >= r).into()),
            (_, _) => return Err(CozoError::InvalidArgument)
        })
    }
    fn le_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                     table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        let (re, right) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null || right == Value::Null {
            return Ok((true, Value::Null));
        }
        if !le || !re {
            return Ok((false, Value::Apply(value::OP_GE.into(), vec![left, right])));
        }
        Ok(match (left, right) {
            (Value::Int(l), Value::Int(r)) => (true, (l <= r).into()),
            (Value::Float(l), Value::Int(r)) => (true, (l <= (r as f64).into()).into()),
            (Value::Int(l), Value::Float(r)) => (true, ((l as f64) <= r.into_inner()).into()),
            (Value::Float(l), Value::Float(r)) => (true, (l <= r).into()),
            (_, _) => return Err(CozoError::InvalidArgument)
        })
    }
    fn mod_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                      table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        let (re, right) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null || right == Value::Null {
            return Ok((true, Value::Null));
        }
        if !le || !re {
            return Ok((false, Value::Apply(value::OP_MOD.into(), vec![left, right])));
        }
        Ok(match (left, right) {
            (Value::Int(l), Value::Int(r)) => (true, (l % r).into()),
            (_, _) => return Err(CozoError::InvalidArgument)
        })
    }
    fn mul_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                      table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        let (re, right) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null || right == Value::Null {
            return Ok((true, Value::Null));
        }
        if !le || !re {
            return Ok((false, Value::Apply(value::OP_MUL.into(), vec![left, right])));
        }
        Ok(match (left, right) {
            (Value::Int(l), Value::Int(r)) => (true, (l * r).into()),
            (Value::Float(l), Value::Int(r)) => (true, (l * (r as f64)).into()),
            (Value::Int(l), Value::Float(r)) => (true, ((l as f64) * r.into_inner()).into()),
            (Value::Float(l), Value::Float(r)) => (true, (l.into_inner() * r.into_inner()).into()),
            (_, _) => return Err(CozoError::InvalidArgument)
        })
    }
    fn div_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                      table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        let (re, right) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null || right == Value::Null {
            return Ok((true, Value::Null));
        }
        if !le || !re {
            return Ok((false, Value::Apply(value::OP_DIV.into(), vec![left, right])));
        }
        Ok(match (left, right) {
            (Value::Int(l), Value::Int(r)) => (true, (l as f64 / r as f64).into()),
            (Value::Float(l), Value::Int(r)) => (true, (l / (r as f64)).into()),
            (Value::Int(l), Value::Float(r)) => (true, ((l as f64) / r.into_inner()).into()),
            (Value::Float(l), Value::Float(r)) => (true, (l.into_inner() / r.into_inner()).into()),
            (_, _) => return Err(CozoError::InvalidArgument)
        })
    }
    fn eq_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                     table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        let (re, right) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null || right == Value::Null {
            return Ok((true, Value::Null));
        }
        if !le || !re {
            return Ok((false, Value::Apply(value::OP_EQ.into(), vec![left, right])));
        }
        Ok((true, (left == right).into()))
    }
    fn ne_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                     table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut args = args.into_iter();
        let (le, left) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        let (re, right) = self.partial_eval(args.next().unwrap(), params, table_bindings)?;
        if left == Value::Null || right == Value::Null {
            return Ok((true, Value::Null));
        }
        if !le || !re {
            return Ok((false, Value::Apply(value::OP_NE.into(), vec![left, right])));
        }
        Ok((true, (left != right).into()))
    }
    fn or_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                     table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let res = args.into_iter().map(|v| self.partial_eval(v, params, table_bindings))
            .try_fold(
                (true, false, vec![]),
                |(is_evaluated, has_null, mut collected), x| {
                    match x {
                        Ok((cur_eval, cur_val)) => {
                            if cur_eval {
                                match cur_val {
                                    Value::Null => {
                                        Ok((is_evaluated, true, collected))
                                    }
                                    Value::Bool(b) => if b {
                                        Err(Ok((true, Value::Bool(true)))) // Early return on true
                                    } else {
                                        Ok((is_evaluated, has_null, collected))
                                    },
                                    _ => Err(Err(CozoError::InvalidArgument))
                                }
                            } else {
                                match cur_val {
                                    Value::Null |
                                    Value::Bool(_) |
                                    Value::Int(_) |
                                    Value::Float(_) |
                                    Value::Uuid(_) |
                                    Value::EndSentinel |
                                    Value::Text(_) => unreachable!(),
                                    Value::List(_) |
                                    Value::Dict(_) => Err(Err(CozoError::InvalidArgument)),
                                    cur_val @ (Value::Variable(_) |
                                    Value::IdxAccess(_, _) |
                                    Value::FieldAccess(_, _) |
                                    Value::Apply(_, _)) => {
                                        collected.push(cur_val);
                                        Ok((false, has_null, collected))
                                    }
                                }
                            }
                        }
                        Err(e) => Err(Err(e))
                    }
                });
        match res {
            Ok((is_evaluated, has_null, mut unevaluated)) => {
                if is_evaluated {
                    if has_null {
                        Ok((true, Value::Null))
                    } else {
                        Ok((true, Value::Bool(false)))
                    }
                } else {
                    if has_null {
                        unevaluated.push(Value::Null);
                    }
                    Ok((false, Value::Apply(value::OP_OR.into(), unevaluated)))
                }
            }
            Err(Ok(res)) => Ok(res),
            Err(Err(e)) => Err(e)
        }
    }
    fn concat_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                         table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let mut total_ret = vec![];
        let mut cur_ret = vec![];
        let mut evaluated = true;
        for val in args.into_iter() {
            let (ev, val) = self.partial_eval(val, params, table_bindings)?;
            evaluated = ev && evaluated;
            match val {
                Value::List(l) => {
                    if cur_ret.is_empty() {
                        cur_ret = l;
                    } else {
                        cur_ret.extend(l);
                    }
                }
                v @ (Value::Variable(_) |
                Value::Apply(_, _) |
                Value::FieldAccess(_, _) |
                Value::IdxAccess(_, _)) => {
                    if !cur_ret.is_empty() {
                        total_ret.push(Value::List(cur_ret));
                        cur_ret = vec![];
                    }
                    total_ret.push(v);
                }
                _ => {
                    return Err(LogicError("Cannot concat incompatible types".to_string()));
                }
            }
        }
        if total_ret.is_empty() {
            Ok((evaluated, cur_ret.into()))
        } else {
            if !cur_ret.is_empty() {
                total_ret.push(cur_ret.into());
            }
            Ok((false, Value::Apply(METHOD_CONCAT.into(), total_ret)))
        }
    }
    fn and_values<'a>(&self, args: Vec<Value<'a>>, params: &BTreeMap<String, Value<'a>>,
                      table_bindings: &BTreeMap<String, ()>) -> Result<(bool, Value<'a>)> {
        let res = args.into_iter().map(|v| self.partial_eval(v, params, table_bindings))
            .try_fold(
                (true, false, vec![]),
                |(is_evaluated, has_null, mut collected), x| {
                    match x {
                        Ok((cur_eval, cur_val)) => {
                            if cur_eval {
                                match cur_val {
                                    Value::Null => {
                                        Ok((is_evaluated, true, collected))
                                    }
                                    Value::Bool(b) => if b {
                                        Ok((is_evaluated, has_null, collected))
                                    } else {
                                        Err(Ok((true, Value::Bool(false)))) // Early return on true
                                    },
                                    _ => Err(Err(CozoError::InvalidArgument))
                                }
                            } else {
                                match cur_val {
                                    Value::Null |
                                    Value::Bool(_) |
                                    Value::Int(_) |
                                    Value::Float(_) |
                                    Value::Uuid(_) |
                                    Value::EndSentinel |
                                    Value::Text(_) => unreachable!(),
                                    Value::List(_) |
                                    Value::Dict(_) => Err(Err(CozoError::InvalidArgument)),
                                    cur_val @ (Value::Variable(_) |
                                    Value::IdxAccess(_, _) |
                                    Value::FieldAccess(_, _) |
                                    Value::Apply(_, _)) => {
                                        collected.push(cur_val);
                                        Ok((false, has_null, collected))
                                    }
                                }
                            }
                        }
                        Err(e) => Err(Err(e))
                    }
                });
        match res {
            Ok((is_evaluated, has_null, mut unevaluated)) => {
                if is_evaluated {
                    if has_null {
                        Ok((true, Value::Null))
                    } else {
                        Ok((true, Value::Bool(true)))
                    }
                } else {
                    if has_null {
                        unevaluated.push(Value::Null);
                    }
                    Ok((false, Value::Apply(value::OP_AND.into(), unevaluated)))
                }
            }
            Err(Ok(res)) => Ok(res),
            Err(Err(e)) => Err(e)
        }
    }


    fn get_next_storage_id(&mut self, in_root: bool) -> Result<i64> {
        // TODO: deal with wrapping problem
        let mut key_entry = Tuple::with_null_prefix();
        key_entry.push_null();
        let db_res = if in_root {
            self.txn.get(true, &self.perm_cf, &key_entry)
        } else {
            self.txn.get(false, &self.temp_cf, &key_entry)
        };
        let u = if let Some(en) = db_res? {
            if let Value::Int(u) = Tuple::new(en).get(0).unwrap() {
                u
            } else {
                panic!("Unexpected value in storage id");
            }
        } else { 0 };
        let mut new_data = Tuple::with_null_prefix();
        new_data.push_int(u + 1);
        if in_root {
            self.txn.put(true, &self.perm_cf, key_entry, new_data)?;
        } else {
            self.txn.put(false, &self.temp_cf, key_entry, new_data)?;
        }
        Ok(u + 1)
    }

    fn get_stack_depth(&self) -> i32 {
        self.stack_depth
    }

    pub fn push_env(&mut self) -> Result<()> {
        if self.stack_depth <= -1024 {
            return Err(CozoError::LogicError("Stack overflow in env".to_string()));
        }
        self.stack_depth -= 1;
        Ok(())
    }

    pub fn pop_env(&mut self) -> Result<()> {
        // Remove all stuff starting with the stack depth from the temp session
        let mut prefix = Tuple::with_null_prefix();
        prefix.push_int(self.stack_depth as i64);
        let it = self.txn.iterator(false, &self.temp_cf);
        it.seek(&prefix);
        let mut to_delete = vec![];
        for val in it.keys() {
            let cur = Tuple::new(val);
            if cur.starts_with(&prefix) {
                if let Some(name) = cur.get(1) {
                    let mut ikey = Tuple::with_null_prefix();
                    ikey.push_value(&name);
                    ikey.push_int(self.stack_depth as i64);

                    let data = self.txn.get(false, &self.temp_cf, &ikey)?
                        .ok_or_else(|| CozoError::LogicError("Bad format for ikey".to_string()))?;
                    let data = Tuple::new(data);
                    match data.data_kind()? {
                        DataKind::Node |
                        DataKind::Edge |
                        DataKind::Assoc |
                        DataKind::Index => {
                            let id = data.get_int(1).ok_or_else(|| CozoError::LogicError("Bad table index".to_string()))?;
                            let mut rkey = Tuple::with_null_prefix();
                            rkey.push_bool(true);
                            rkey.push_int(id);
                            self.txn.del(false, &self.temp_cf, rkey)?;
                            let range_start = Tuple::with_prefix(id as u32);
                            let mut range_end = Tuple::with_prefix(id as u32);
                            range_end.seal_with_sentinel();
                            self.txn.del_range(&self.temp_cf, range_start, range_end)?;
                        }
                        _ => {}
                    }
                    to_delete.push(cur.data.as_ref().to_vec());
                    to_delete.push(ikey.data.to_vec());
                }
            } else {
                break;
            }
        }

        let mut prefix = Tuple::with_null_prefix();
        prefix.push_null();
        prefix.push_int(self.stack_depth as i64);
        let it = self.txn.iterator(false, &self.temp_cf);
        it.seek(&prefix);
        for val in it.keys() {
            let cur = Tuple::new(val);
            if cur.starts_with(&prefix) {
                let mut ikey = Tuple::with_prefix(cur.get_prefix());
                ikey.push_null();
                ikey.push_str(cur.get_text(2).unwrap());
                ikey.push_int(cur.get_int(1).unwrap());
                for k in cur.iter().skip(3) {
                    ikey.push_value(&k);
                }

                to_delete.push(cur.data.as_ref().to_vec());
                to_delete.push(ikey.data.to_vec());
            } else {
                break;
            }
        }

        if self.stack_depth != 0 {
            self.stack_depth += 1;
        }

        for d in to_delete {
            self.txn.del(false, &self.temp_cf, &d)?;
        }

        Ok(())
    }

    pub fn table_data(&self, id: i64, in_root: bool) -> Result<Option<Tuple<SlicePtr>>> {
        let mut key = Tuple::with_null_prefix();
        key.push_bool(true);
        key.push_int(id);
        if in_root {
            let data = self.txn.get(true, &self.perm_cf, key)?;
            Ok(data.map(Tuple::new))
        } else {
            let data = self.txn.get(false, &self.temp_cf, key)?;
            Ok(data.map(Tuple::new))
        }
    }

    pub fn resolve(&self, name: &str) -> Result<Option<Tuple<SlicePtr>>> {
        let mut tuple = Tuple::with_null_prefix();
        tuple.push_str(name);
        let it = self.txn.iterator(false, &self.temp_cf);
        it.seek(&tuple);
        if let Some((tk, vk)) = it.pair() {
            let k = Tuple::new(tk);
            if k.starts_with(&tuple) {
                return Ok(Some(Tuple::new(vk)));
            }
        }
        let root_key = self.encode_definable_key(name, true);
        let res = self.txn.get(true, &self.perm_cf, root_key).map(|v| v.map(Tuple::new))?;
        Ok(res)
    }

    pub fn resolve_related_tables(&self, name: &str) -> Result<Vec<(String, Tuple<SlicePtr>)>> {
        let mut prefix = Tuple::with_prefix(0);
        prefix.push_null();
        prefix.push_str(name);
        let mut assocs = vec![];

        let it = self.txn.iterator(true, &self.perm_cf);
        it.seek(&prefix);
        for val in it.keys() {
            let cur = Tuple::new(val);
            if !cur.starts_with(&prefix) {
                break;
            }
            let name = cur.get_text(4).ok_or_else(|| LogicError("Bad data".to_string()))?;
            if let Some(data) = self.resolve(&name)? {
                if data.data_kind()? == DataKind::Assoc {
                    assocs.push((name.to_string(), data));
                }
            }
        }

        let it = self.txn.iterator(false, &self.temp_cf);
        it.seek(&prefix);
        for val in it.keys() {
            let cur = Tuple::new(val);
            if !cur.starts_with(&prefix) {
                break;
            }
            let name = cur.get_text(4).ok_or_else(|| LogicError("Bad data".to_string()))?;
            if let Some(data) = self.resolve(&name)? {
                if data.data_kind()? == DataKind::Assoc {
                    assocs.push((name.to_string(), data));
                }
            }
        }

        Ok(assocs)
    }

    pub fn delete_defined(&mut self, name: &str, in_root: bool) -> Result<()> {
        let key = self.encode_definable_key(name, in_root);
        if in_root {
            self.txn.del(true, &self.perm_cf, key)?;
        } else {
            let it = self.txn.iterator(false, &self.temp_cf);
            it.seek(&key);
            if let Some(found_key) = it.key() {
                let found_key_tuple = Tuple::new(found_key);
                if found_key_tuple.starts_with(&key) {
                    let mut ikey = Tuple::with_null_prefix();
                    ikey.push_value(&found_key_tuple.get(1).unwrap());
                    ikey.push_value(&found_key_tuple.get(0).unwrap());
                    self.txn.del(false, &self.temp_cf, found_key_tuple)?;
                    self.txn.del(false, &self.temp_cf, ikey)?;
                }
            }
        }
        // TODO cleanup if the thing deleted is a table

        Ok(())
    }

    fn define_data(&mut self, name: &str, data: OwnTuple, in_root: bool) -> Result<()> {
        let key = self.encode_definable_key(name, in_root);
        if in_root {
            self.txn.put(true, &self.perm_cf, key, data)?;
        } else {
            let mut ikey = Tuple::with_null_prefix();
            ikey.push_int(self.stack_depth as i64);
            ikey.push_str(name);
            self.txn.put(false, &self.temp_cf, key, data)?;
            self.txn.put(false, &self.temp_cf, ikey, "")?;
        }
        Ok(())
    }

    pub fn key_exists(&self, key: &OwnTuple, in_root: bool) -> Result<bool> {
        let res = self.txn.get(in_root, if in_root { &self.perm_cf } else { &self.temp_cf }, key)?;
        Ok(res.is_some())
    }

    pub fn del_key(&self, key: &OwnTuple, in_root: bool) -> Result<()> {
        self.txn.del(in_root, if in_root { &self.perm_cf } else { &self.temp_cf }, key)?;
        Ok(())
    }

    pub fn define_raw_key(&self, key: &OwnTuple, value: Option<&OwnTuple>, in_root: bool) -> Result<()> {
        if in_root {
            match value {
                None => {
                    self.txn.put(true, &self.perm_cf, key, "")?;
                }
                Some(v) => {
                    self.txn.put(true, &self.perm_cf, key, &v)?;
                }
            }
        } else {
            match value {
                None => {
                    self.txn.put(false, &self.temp_cf, key, "")?;
                }
                Some(v) => {
                    self.txn.put(false, &self.temp_cf, key, &v)?;
                }
            }
        }
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use std::fs;
    use super::*;
    use crate::parser::{Parser, Rule};
    use pest::Parser as PestParser;
    use crate::db::engine::Engine;

    #[test]
    fn node() {
        let s = r#"
            create node "Person" {
                *id: Int,
                name: Text,
                email: ?Text,
                habits: ?[?Text]
            }

            create edge (Person)-[Friend]->(Person) {
                relation: ?Text
            }

            create type XXY = {me: Int, f: ?[Text]}

            local assoc WorkInfo: Person {
                email: Text
            }
        "#;
        let db_path = "_test_node";
        {
            let engine = Engine::new(db_path.to_string(), true).unwrap();
            let mut env = engine.session().unwrap();

            let mut parsed = Parser::parse(Rule::file, s).unwrap();

            let t = parsed.next().unwrap();
            env.run_definition(t).unwrap();
            println!("{:?}", env.resolve("Person"));

            let t = parsed.next().unwrap();
            env.run_definition(t).unwrap();
            println!("{:?}", env.resolve("Friend"));

            let t = parsed.next().unwrap();
            env.run_definition(t).unwrap();
            println!("{:?}", env.resolve("XXY"));

            let t = parsed.next().unwrap();
            env.run_definition(t).unwrap();
            println!("{:?}", env.resolve("WorkInfo"));
            println!("{:?}", env.resolve("Person"));
            env.commit().unwrap();

            let it = env.txn.iterator(false, &env.perm_cf);
            it.to_first();
            for (k, v) in it.iter() {
                println!("{:?}, {:?}", Tuple::new(k), Tuple::new(v));
            }

            let it = env.txn.iterator(false, &env.temp_cf);
            it.to_first();
            for (k, v) in it.iter() {
                println!("{:?}, {:?}", Tuple::new(k), Tuple::new(v));
            }
        }
        fs::remove_dir_all(db_path).unwrap();
    }

    #[test]
    fn eval_expr() {
        let db_path = "_test_db_expr_eval";
        let engine = Engine::new(db_path.to_string(), true).unwrap();
        let sess = engine.session().unwrap();

        let parse_expr_from_str = |s: &str| -> (bool, Value) {
            let (b, v) = sess.partial_eval(
                Value::from_pair(Parser::parse(Rule::expr, s)
                    .unwrap().next().unwrap()).unwrap(),
                &Default::default(), &Default::default()).unwrap();
            (b, v.to_static())
        };

        assert_eq!((true, Value::from(1024.1)), parse_expr_from_str("1/10+(-2+3)*4^5"));
        assert_eq!((true, Value::from(false)), parse_expr_from_str("true && false"));
        assert_eq!((true, Value::from(true)), parse_expr_from_str("true || false"));
        assert_eq!((true, Value::from(true)), parse_expr_from_str("true || null"));
        assert_eq!((true, Value::from(true)), parse_expr_from_str("null || true"));
        assert_eq!((true, Value::Null), parse_expr_from_str("true && null"));
        let ex = parse_expr_from_str("a + b - 1*2*3*100*c * d");
        println!("{:?} {}", ex.0, ex.1);
        drop(sess);
        drop(engine);
        fs::remove_dir_all(db_path).unwrap();
    }
}

//     fn test_null_expr<'a>(&self, exprs: &[Expr<'a>]) -> Result<Expr<'a>> {
//         Ok(match exprs {
//             [a] => {
//                 match self.visit_expr(a)? {
//                     Const(Null) => Const(Bool(true)),
//                     Const(_) => Const(Bool(false)),
//                     v => Value::Apply(Op::IsNull, vec![v])
//                 }
//             }
//             _ => unreachable!()
//         })
//     }
//
//     fn not_null_expr<'a>(&self, exprs: &[Expr<'a>]) -> Result<Expr<'a>> {
//         Ok(match exprs {
//             [a] => {
//                 match self.visit_expr(a)? {
//                     Const(Null) => Const(Bool(false)),
//                     Const(_) => Const(Bool(true)),
//                     v => Value::Apply(Op::IsNull, vec![v])
//                 }
//             }
//             _ => unreachable!()
//         })
//     }
// #[cfg(test)]
// mod tests {
//     use super::*;
//
//     #[test]
//     fn operators() {
//         let ev = Evaluator::new(DummyStorage {}).unwrap();
//
//     }
// }