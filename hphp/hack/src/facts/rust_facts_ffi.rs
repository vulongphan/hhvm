// Copyright (c) 2019, Facebook, Inc.
// All rights reserved.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the "hack" directory of this source tree.

use hhbc_by_ref_hhbc_string_utils::without_xhp_mangling;
use ocamlrep::{bytes_from_ocamlrep, ptr::UnsafeOcamlPtr};
use ocamlrep_ocamlpool::ocaml_ffi;
use oxidized::relative_path::RelativePath;

use facts_rust::{facts::*, facts_parser::*};
ocaml_ffi! {
    fn extract_as_json_ffi(
        flags: i32,
        filename: RelativePath,
        text_ptr: UnsafeOcamlPtr,
        mangle_xhp: bool,
    ) -> Option<String> {
        // Safety: the OCaml garbage collector must not run as long as text_ptr
        // and text_value exist. We don't call into OCaml here, so it won't.
        let text_value = unsafe { text_ptr.as_value() };
        let text = bytes_from_ocamlrep(text_value).expect("expected string");
        extract_facts_as_json_ffi0(
            ((1 << 0) & flags) != 0, // php5_compat_mode
            ((1 << 1) & flags) != 0, // hhvm_compat_mode
            ((1 << 2) & flags) != 0, // allow_new_attribute_syntax
            ((1 << 3) & flags) != 0, // enable_xhp_class_modifier
            ((1 << 4) & flags) != 0, // disable_xhp_element_mangling
            ((1 << 5) & flags) != 0, // disallow_hash_comments
            filename,
            text,
            mangle_xhp,
        )
    }
}

pub fn extract_facts_as_json_ffi0(
    php5_compat_mode: bool,
    hhvm_compat_mode: bool,
    allow_new_attribute_syntax: bool,
    enable_xhp_class_modifier: bool,
    disable_xhp_element_mangling: bool,
    disallow_hash_comments: bool,
    filename: RelativePath,
    text: &[u8],
    mangle_xhp: bool,
) -> Option<String> {
    let opts = FactsOpts {
        php5_compat_mode,
        hhvm_compat_mode,
        allow_new_attribute_syntax,
        enable_xhp_class_modifier,
        disable_xhp_element_mangling,
        filename,
        disallow_hash_comments,
    };
    if mangle_xhp {
        extract_as_json(text, opts)
    } else {
        without_xhp_mangling(|| extract_as_json(text, opts))
    }
}

pub fn extract_facts_ffi0(
    php5_compat_mode: bool,
    hhvm_compat_mode: bool,
    allow_new_attribute_syntax: bool,
    enable_xhp_class_modifier: bool,
    disable_xhp_element_mangling: bool,
    disallow_hash_comments: bool,
    filename: RelativePath,
    text: &[u8],
    _mangle_xhp: bool,
) -> Option<Facts> {
    let opts = FactsOpts {
        php5_compat_mode,
        hhvm_compat_mode,
        allow_new_attribute_syntax,
        enable_xhp_class_modifier,
        disable_xhp_element_mangling,
        filename,
        disallow_hash_comments,
    };
    from_text(text, opts)
}

pub fn facts_to_json_ffi(facts: Facts, text: &[u8]) -> String {
    facts.to_json(text)
}
