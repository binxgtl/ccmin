p = 'src/schema.rs'
s = open(p, encoding='utf-8').read()


def sub(old, new):
    global s
    assert s.count(old) == 1, ("MISSING: " + old[:70], s.count(old))
    s = s.replace(old, new)


# --- producer #2, inside the existing worklist --------------------------
sub("""            if state.narrow(target.clone(), &induced, &domain) {
                queue.push_back(target);
            }
        }
    }

    state
}""",
    """            if state.narrow(target.clone(), &induced, &domain) {
                queue.push_back(target);
            }
        }

        // Producer #2. An `index` whose target axis just narrowed loses every
        // reference to a position that no longer survives. Same event, same
        // merge, same queue -- `Index` is a second producer, not a second
        // mechanism.
        let Some(block) = block_at(&schema.items, &key.0) else {
            continue;
        };
        for (i, decl) in block.iter().enumerate() {
            let Decl::Index { len, target, .. } = decl else {
                continue;
            };
            if schema.sizing_axis(target) != Some(key.1) {
                continue;
            }
            let Some(index_axis) = schema.sizing_axis(len) else {
                continue;
            };
            let mut path = key.0.clone();
            path.push(i);
            // ORIGINAL references plus the current source mask. Never a
            // projection: the values are positions in the target's original
            // domain, and projected data no longer carries that identity.
            let Some(Value::Array(refs)) = value_at(&data.values, &path) else {
                continue;
            };
            let survivors: Vec<usize> = refs
                .iter()
                .enumerate()
                .filter(|(_, v)| match usize::try_from(**v - 1) {
                    Ok(position) => source.binary_search(&position).is_ok(),
                    Err(_) => false,
                })
                .map(|(position, _)| position)
                .collect();

            let target_key: OccurrenceKey = (key.0.clone(), index_axis);
            let Some(target_occ) = all.iter().find(|o| o.prefix == key.0 && o.axis == index_axis)
            else {
                continue;
            };
            let Some(extent) = occurrence_extent(data, target_occ) else {
                continue;
            };
            let domain: Vec<usize> = (0..extent).collect();
            if state.narrow(target_key.clone(), &survivors, &domain) {
                queue.push_back(target_key);
            }
        }
    }

    state
}

/// Rewrite surviving references from original target positions to projected
/// ones.
///
/// Derived from the final keep-mask, never maintained during propagation. A
/// reference whose target did not survive means the induction missed it: the
/// candidate is rejected rather than clipped, renumbered onto a neighbour, or
/// left dangling.
fn renumber_indices(
    schema: &Schema,
    items: &[Decl],
    trial: &mut SchemaData,
    prefix: &Path,
    masks: &BTreeMap<OccurrenceKey, Vec<usize>>,
) -> Option<()> {
    for (i, decl) in items.iter().enumerate() {
        let mut path = prefix.clone();
        path.push(i);
        match decl {
            Decl::Index { target, .. } => {
                let Some(axis) = schema.sizing_axis(target) else {
                    continue;
                };
                let Some(mask) = masks.get(&(prefix.clone(), axis)) else {
                    continue; // the target was untouched, so labels still hold
                };
                let Some(Value::Array(refs)) = value_at(&trial.values, &path).cloned() else {
                    continue;
                };
                let mut rewritten = Vec::with_capacity(refs.len());
                for reference in refs {
                    let old = usize::try_from(reference - 1).ok()?;
                    // Position within the survivors, one-based. `None` here is
                    // a dangling reference: reject.
                    let new = mask.binary_search(&old).ok()? + 1;
                    rewritten.push(new as i64);
                }
                put(trial, &path, Value::Array(rewritten));
            }
            Decl::Repeat { body, .. } => {
                let iterations = match value_at(&trial.values, &path) {
                    Some(Value::Repeat(iters)) => iters.len(),
                    _ => 0,
                };
                for k in 0..iterations {
                    let mut inner = path.clone();
                    inner.push(k);
                    renumber_indices(schema, body, trial, &inner, masks)?;
                }
            }
            _ => {}
        }
    }
    Some(())
}""")

# --- projection applies the mapping once the fixpoint is known ----------
sub("""    let mut trial = data.clone();
    for (path, ops) in edits {
        let original = value_at(&data.values, &path)?;
        let projected = apply_masks(original, &ops)?;
        put(&mut trial, &path, projected);
    }""",
    """    let mut trial = data.clone();
    for (path, ops) in edits {
        let original = value_at(&data.values, &path)?;
        let projected = apply_masks(original, &ops)?;
        put(&mut trial, &path, projected);
    }

    // Only now, with the final masks known.
    let schema = Rc::clone(&data.schema);
    renumber_indices(&schema, &schema.items, &mut trial, &Vec::new(), &state.masks)?;""")

# --- the consistency helper needs to know about index arrays ------------
sub("""            (Decl::Array { len, .. }, Value::Array(arr)) => expect(len, arr.len(), "the array"),""",
    """            (Decl::Array { len, .. }, Value::Array(arr)) => expect(len, arr.len(), "the array"),
            (Decl::Index { len, .. }, Value::Array(refs)) => {
                expect(len, refs.len(), "the index array")
            }""")

open(p, 'w', encoding='utf-8', newline='\n').write(s)
print("index induction and renumbering applied")
