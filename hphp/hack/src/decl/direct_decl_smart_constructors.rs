// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the "hack" directory of this source tree.

use std::collections::BTreeMap;
use std::rc::Rc;

use bstr::BStr;
use bumpalo::{
    collections::{String, Vec},
    Bump,
};

use hh_autoimport_rust as hh_autoimport;
use naming_special_names_rust as naming_special_names;

use arena_collections::{AssocListMut, MultiSetMut};
use flatten_smart_constructors::{FlattenOp, FlattenSmartConstructors};
use namespaces::ElaborateKind;
use namespaces_rust as namespaces;
use oxidized_by_ref::{
    aast,
    ast_defs::{
        Abstraction, Bop, ClassishKind, ConstraintKind, FunKind, Id, ShapeFieldName, Uop, Variance,
        XhpEnumValue,
    },
    decl_parser_options::DeclParserOptions,
    direct_decl_parser::Decls,
    file_info::Mode,
    method_flags::MethodFlags,
    namespace_env::Env as NamespaceEnv,
    nast,
    pos::Pos,
    prop_flags::PropFlags,
    relative_path::RelativePath,
    s_map::SMap,
    shallow_decl_defs::{
        self, Decl, ShallowClassConst, ShallowMethod, ShallowProp, ShallowTypeconst,
    },
    shape_map::ShapeField,
    t_shape_map::TShapeField,
    typing_defs::{
        self, AbstractTypeconst, Capability::*, ClassConstKind, ConcreteTypeconst, ConstDecl,
        Enforcement, EnumType, FunArity, FunElt, FunImplicitParams, FunParam, FunParams, FunType,
        IfcFunDecl, ParamMode, PosByteString, PosId, PosString, PossiblyEnforcedTy, RecordFieldReq,
        ShapeFieldType, ShapeKind, TaccessType, Tparam, TshapeFieldName, Ty, Ty_, Typeconst,
        TypedefType, WhereConstraint, XhpAttrTag,
    },
    typing_defs_flags::{FunParamFlags, FunTypeFlags},
    typing_modules::Module_,
    typing_reason::Reason,
};
use parser_core_types::{
    compact_token::CompactToken, indexed_source_text::IndexedSourceText, source_text::SourceText,
    syntax_kind::SyntaxKind, token_factory::SimpleTokenFactoryImpl, token_kind::TokenKind,
};

mod direct_decl_smart_constructors_generated;

type SK = SyntaxKind;

type SSet<'a> = arena_collections::SortedSet<'a, &'a str>;

#[derive(Clone)]
pub struct DirectDeclSmartConstructors<'a, 'text, S: SourceTextAllocator<'text, 'a>> {
    pub token_factory: SimpleTokenFactoryImpl<CompactToken>,

    pub source_text: IndexedSourceText<'text>,
    pub arena: &'a bumpalo::Bump,
    pub decls: Decls<'a>,
    // const_refs will accumulate all scope-resolution-expressions it enconuters while it's "Some"
    const_refs: Option<arena_collections::set::Set<'a, typing_defs::ClassConstRef<'a>>>,
    opts: &'a DeclParserOptions<'a>,
    filename: &'a RelativePath<'a>,
    file_mode: Mode,
    namespace_builder: Rc<NamespaceBuilder<'a>>,
    classish_name_builder: ClassishNameBuilder<'a>,
    type_parameters: Rc<Vec<'a, SSet<'a>>>,

    previous_token_kind: TokenKind,

    source_text_allocator: S,
}

impl<'a, 'text, S: SourceTextAllocator<'text, 'a>> DirectDeclSmartConstructors<'a, 'text, S> {
    pub fn new(
        opts: &'a DeclParserOptions<'a>,
        src: &SourceText<'text>,
        file_mode: Mode,
        arena: &'a Bump,
        source_text_allocator: S,
    ) -> Self {
        let source_text = IndexedSourceText::new(src.clone());
        let path = source_text.source_text().file_path();
        let prefix = path.prefix();
        let path = String::from_str_in(path.path_str(), arena).into_bump_str();
        let filename = RelativePath::make(prefix, path);
        Self {
            token_factory: SimpleTokenFactoryImpl::new(),

            source_text,
            arena,
            opts,
            filename: arena.alloc(filename),
            file_mode,
            decls: Decls::empty(),
            const_refs: None,
            namespace_builder: Rc::new(NamespaceBuilder::new_in(
                opts.auto_namespace_map,
                opts.disable_xhp_element_mangling,
                arena,
            )),
            classish_name_builder: ClassishNameBuilder::new(),
            type_parameters: Rc::new(Vec::new_in(arena)),
            // EndOfFile is used here as a None value (signifying "beginning of
            // file") to save space. There is no legitimate circumstance where
            // we would parse a token and the previous token kind would be
            // EndOfFile.
            previous_token_kind: TokenKind::EndOfFile,
            source_text_allocator,
        }
    }

    #[inline(always)]
    pub fn alloc<T>(&self, val: T) -> &'a T {
        self.arena.alloc(val)
    }

    fn qualified_name_from_parts(&self, parts: &'a [Node<'a>], pos: &'a Pos<'a>) -> Id<'a> {
        // Count the length of the qualified name, so that we can allocate
        // exactly the right amount of space for it in our arena.
        let mut len = 0;
        for part in parts {
            match part {
                Node::Name(&(name, _)) => len += name.len(),
                Node::Token(t) if t.kind() == TokenKind::Backslash => len += 1,
                Node::ListItem(&(Node::Name(&(name, _)), _backslash)) => len += name.len() + 1,
                Node::ListItem(&(Node::Token(t), _backslash))
                    if t.kind() == TokenKind::Namespace =>
                {
                    len += t.width() + 1;
                }
                _ => {}
            }
        }
        // If there's no internal trivia, then we can just reference the
        // qualified name in the original source text instead of copying it.
        let source_len = pos.end_cnum() - pos.start_cnum();
        if source_len == len {
            let qualified_name: &'a str = self.str_from_utf8(self.source_text_at_pos(pos));
            return Id(pos, qualified_name);
        }
        // Allocate `len` bytes and fill them with the fully qualified name.
        let mut qualified_name = String::with_capacity_in(len, self.arena);
        for part in parts {
            match part {
                Node::Name(&(name, _pos)) => qualified_name.push_str(&name),
                Node::Token(t) if t.kind() == TokenKind::Backslash => qualified_name.push('\\'),
                &Node::ListItem(&(Node::Name(&(name, _)), _backslash)) => {
                    qualified_name.push_str(&name);
                    qualified_name.push_str("\\");
                }
                &Node::ListItem(&(Node::Token(t), _backslash))
                    if t.kind() == TokenKind::Namespace =>
                {
                    qualified_name.push_str("namespace\\");
                }
                _ => {}
            }
        }
        debug_assert_eq!(len, qualified_name.len());
        debug_assert_eq!(len, qualified_name.capacity());
        Id(pos, qualified_name.into_bump_str())
    }

    /// If the given node is an identifier, XHP name, or qualified name,
    /// elaborate it in the current namespace and return Some. To be used for
    /// the name of a decl in its definition (e.g., "C" in `class C {}` or "f"
    /// in `function f() {}`).
    fn elaborate_defined_id(&self, name: Node<'a>) -> Option<Id<'a>> {
        let id = match name {
            Node::Name(&(name, pos)) => Id(pos, name),
            Node::XhpName(&(name, pos)) => Id(pos, name),
            Node::QualifiedName(&(parts, pos)) => self.qualified_name_from_parts(parts, pos),
            _ => return None,
        };
        Some(self.namespace_builder.elaborate_defined_id(id))
    }

    /// If the given node is a name (i.e., an identifier or a qualified name),
    /// return Some. No namespace elaboration is performed.
    fn expect_name(&self, name: Node<'a>) -> Option<Id<'a>> {
        // If it's a simple identifier, return it.
        if let id @ Some(_) = name.as_id() {
            return id;
        }
        match name {
            Node::QualifiedName(&(parts, pos)) => Some(self.qualified_name_from_parts(parts, pos)),
            Node::Token(t) if t.kind() == TokenKind::XHP => {
                let pos = self.token_pos(t);
                let text = self.str_from_utf8(self.source_text_at_pos(pos));
                Some(Id(pos, text))
            }
            _ => None,
        }
    }

    /// Fully qualify the given identifier as a type name (with consideration
    /// to `use` statements in scope).
    fn elaborate_id(&self, id: Id<'a>) -> Id<'a> {
        let Id(pos, name) = id;
        Id(pos, self.elaborate_raw_id(name))
    }

    /// Fully qualify the given identifier as a type name (with consideration
    /// to `use` statements in scope).
    fn elaborate_raw_id(&self, id: &'a str) -> &'a str {
        self.namespace_builder
            .elaborate_raw_id(ElaborateKind::Class, id)
    }

    /// Fully qualify the given identifier as a constant name (with
    /// consideration to `use` statements in scope).
    fn elaborate_const_id(&self, id: Id<'a>) -> Id<'a> {
        let Id(pos, name) = id;
        Id(
            pos,
            self.namespace_builder
                .elaborate_raw_id(ElaborateKind::Const, name),
        )
    }

    fn slice<T>(&self, iter: impl Iterator<Item = T>) -> &'a [T] {
        let mut result = match iter.size_hint().1 {
            Some(upper_bound) => Vec::with_capacity_in(upper_bound, self.arena),
            None => Vec::new_in(self.arena),
        };
        for item in iter {
            result.push(item);
        }
        result.into_bump_slice()
    }

    fn start_accumulating_const_refs(&mut self) {
        self.const_refs = Some(arena_collections::set::Set::empty());
    }

    fn accumulate_const_ref(&mut self, class_id: &'a aast::ClassId<'_, (), ()>, value_id: &Id<'a>) {
        // The decl for a class constant stores a list of all the scope-resolution expressions
        // it contains. For example "const C=A::X" stores A::X, and "const D=self::Y" stores self::Y.
        // (This is so we can detect cross-type circularity in constant initializers).
        // TODO: Hack is the wrong place to detect circularity (because we can never do it completely soundly,
        // and because it's a cross-body problem). The right place to do it is in a linter. All this should be
        // removed from here and put into a linter.
        if let Some(const_refs) = self.const_refs {
            match class_id.2 {
                nast::ClassId_::CI(sid) => {
                    self.const_refs = Some(const_refs.add(
                        self.arena,
                        typing_defs::ClassConstRef(
                            typing_defs::ClassConstFrom::From(sid.1),
                            value_id.1,
                        ),
                    ));
                }
                nast::ClassId_::CIself => {
                    self.const_refs = Some(const_refs.add(
                        self.arena,
                        typing_defs::ClassConstRef(typing_defs::ClassConstFrom::Self_, value_id.1),
                    ));
                }
                // Not allowed
                nast::ClassId_::CIparent
                | nast::ClassId_::CIstatic
                | nast::ClassId_::CIexpr(_) => {}
            }
        }
    }

    fn stop_accumulating_const_refs(&mut self) -> &'a [typing_defs::ClassConstRef<'a>] {
        let const_refs = self.const_refs;
        self.const_refs = None;
        match const_refs {
            Some(const_refs) => {
                let mut elements: Vec<'_, typing_defs::ClassConstRef<'_>> =
                    bumpalo::collections::Vec::with_capacity_in(const_refs.count(), self.arena);
                elements.extend(const_refs.into_iter());
                elements.into_bump_slice()
            }
            None => &[],
        }
    }
}

pub trait SourceTextAllocator<'text, 'target>: Clone {
    fn alloc(&self, text: &'text str) -> &'target str;
}

#[derive(Clone)]
pub struct NoSourceTextAllocator;

impl<'text> SourceTextAllocator<'text, 'text> for NoSourceTextAllocator {
    #[inline]
    fn alloc(&self, text: &'text str) -> &'text str {
        text
    }
}

#[derive(Clone)]
pub struct ArenaSourceTextAllocator<'arena>(pub &'arena bumpalo::Bump);

impl<'text, 'arena> SourceTextAllocator<'text, 'arena> for ArenaSourceTextAllocator<'arena> {
    #[inline]
    fn alloc(&self, text: &'text str) -> &'arena str {
        self.0.alloc_str(text)
    }
}

fn prefix_slash<'a>(arena: &'a Bump, name: &str) -> &'a str {
    let mut s = String::with_capacity_in(1 + name.len(), arena);
    s.push('\\');
    s.push_str(name);
    s.into_bump_str()
}

fn prefix_colon<'a>(arena: &'a Bump, name: &str) -> &'a str {
    let mut s = String::with_capacity_in(1 + name.len(), arena);
    s.push(':');
    s.push_str(name);
    s.into_bump_str()
}

fn concat<'a>(arena: &'a Bump, str1: &str, str2: &str) -> &'a str {
    let mut result = String::with_capacity_in(str1.len() + str2.len(), arena);
    result.push_str(str1);
    result.push_str(str2);
    result.into_bump_str()
}

fn strip_dollar_prefix<'a>(name: &'a str) -> &'a str {
    name.trim_start_matches("$")
}

const TANY_: Ty_<'_> = Ty_::Tany(oxidized_by_ref::tany_sentinel::TanySentinel);
const TANY: &Ty<'_> = &Ty(Reason::none(), TANY_);

fn tany() -> &'static Ty<'static> {
    TANY
}

fn default_ifc_fun_decl<'a>() -> IfcFunDecl<'a> {
    IfcFunDecl::FDPolicied(Some("PUBLIC"))
}

#[derive(Debug)]
struct Modifiers {
    is_static: bool,
    visibility: aast::Visibility,
    is_abstract: bool,
    is_final: bool,
    is_readonly: bool,
}

fn read_member_modifiers<'a: 'b, 'b>(modifiers: impl Iterator<Item = &'b Node<'a>>) -> Modifiers {
    let mut ret = Modifiers {
        is_static: false,
        visibility: aast::Visibility::Public,
        is_abstract: false,
        is_final: false,
        is_readonly: false,
    };
    for modifier in modifiers {
        if let Some(vis) = modifier.as_visibility() {
            ret.visibility = vis;
        }
        match modifier.token_kind() {
            Some(TokenKind::Static) => ret.is_static = true,
            Some(TokenKind::Abstract) => ret.is_abstract = true,
            Some(TokenKind::Final) => ret.is_final = true,
            Some(TokenKind::Readonly) => ret.is_readonly = true,
            _ => {}
        }
    }
    ret
}

#[derive(Clone, Debug)]
struct NamespaceBuilder<'a> {
    arena: &'a Bump,
    stack: Vec<'a, NamespaceEnv<'a>>,
    auto_ns_map: &'a [(&'a str, &'a str)],
}

impl<'a> NamespaceBuilder<'a> {
    fn new_in(
        auto_ns_map: &'a [(&'a str, &'a str)],
        disable_xhp_element_mangling: bool,
        arena: &'a Bump,
    ) -> Self {
        let mut ns_uses = SMap::empty();
        for &alias in hh_autoimport::NAMESPACES {
            ns_uses = ns_uses.add(arena, alias, concat(arena, "HH\\", alias));
        }
        for (alias, ns) in auto_ns_map.iter() {
            ns_uses = ns_uses.add(arena, alias, ns);
        }

        let mut class_uses = SMap::empty();
        for &alias in hh_autoimport::TYPES {
            class_uses = class_uses.add(arena, alias, concat(arena, "HH\\", alias));
        }

        NamespaceBuilder {
            arena,
            stack: bumpalo::vec![in arena; NamespaceEnv {
                ns_uses,
                class_uses,
                fun_uses: SMap::empty(),
                const_uses: SMap::empty(),
                record_def_uses: SMap::empty(),
                name: None,
                auto_ns_map,
                is_codegen: false,
                disable_xhp_element_mangling,
            }],
            auto_ns_map,
        }
    }

    fn push_namespace(&mut self, name: Option<&str>) {
        let current = self.current_namespace();
        let nsenv = self.stack.last().unwrap().clone(); // shallow clone
        if let Some(name) = name {
            let mut fully_qualified = match current {
                None => String::with_capacity_in(name.len(), self.arena),
                Some(current) => {
                    let mut fully_qualified =
                        String::with_capacity_in(current.len() + name.len() + 1, self.arena);
                    fully_qualified.push_str(current);
                    fully_qualified.push('\\');
                    fully_qualified
                }
            };
            fully_qualified.push_str(name);
            self.stack.push(NamespaceEnv {
                name: Some(fully_qualified.into_bump_str()),
                ..nsenv
            });
        } else {
            self.stack.push(NamespaceEnv {
                name: current,
                ..nsenv
            });
        }
    }

    fn pop_namespace(&mut self) {
        // We'll never push a namespace for a declaration of items in the global
        // namespace (e.g., `namespace { ... }`), so only pop if we are in some
        // namespace other than the global one.
        if self.stack.len() > 1 {
            self.stack.pop().unwrap();
        }
    }

    // push_namespace(Y) + pop_namespace() + push_namespace(X) should be equivalent to
    // push_namespace(Y) + push_namespace(X) + pop_previous_namespace()
    fn pop_previous_namespace(&mut self) {
        if self.stack.len() > 2 {
            let last = self.stack.pop().unwrap().name.unwrap_or("\\");
            let previous = self.stack.pop().unwrap().name.unwrap_or("\\");
            assert!(last.starts_with(previous));
            let name = &last[previous.len() + 1..last.len()];
            self.push_namespace(Some(name));
        }
    }

    fn current_namespace(&self) -> Option<&'a str> {
        self.stack.last().and_then(|nsenv| nsenv.name)
    }

    fn add_import(&mut self, kind: NamespaceUseKind, name: &'a str, aliased_name: Option<&'a str>) {
        let stack_top = &mut self
            .stack
            .last_mut()
            .expect("Attempted to get the current import map, but namespace stack was empty");
        let aliased_name = aliased_name.unwrap_or_else(|| {
            name.rsplit_terminator('\\')
                .nth(0)
                .expect("Expected at least one entry in import name")
        });
        let name = name.trim_end_matches('\\');
        let name = if name.starts_with('\\') {
            name
        } else {
            prefix_slash(self.arena, name)
        };
        match kind {
            NamespaceUseKind::Type => {
                stack_top.class_uses = stack_top.class_uses.add(self.arena, aliased_name, name);
            }
            NamespaceUseKind::Namespace => {
                stack_top.ns_uses = stack_top.ns_uses.add(self.arena, aliased_name, name);
            }
            NamespaceUseKind::Mixed => {
                stack_top.class_uses = stack_top.class_uses.add(self.arena, aliased_name, name);
                stack_top.ns_uses = stack_top.ns_uses.add(self.arena, aliased_name, name);
            }
        }
    }

    fn elaborate_raw_id(&self, kind: ElaborateKind, name: &'a str) -> &'a str {
        if name.starts_with('\\') {
            return name;
        }
        let env = self.stack.last().unwrap();
        namespaces::elaborate_raw_id_in(self.arena, env, kind, name)
    }

    fn elaborate_defined_id(&self, id: Id<'a>) -> Id<'a> {
        let Id(pos, name) = id;
        let env = self.stack.last().unwrap();
        let name = if env.disable_xhp_element_mangling && name.contains(':') {
            let xhp_name_opt = namespaces::elaborate_xhp_namespace(name);
            let name = xhp_name_opt.map_or(name, |s| self.arena.alloc_str(&s));
            if !name.starts_with('\\') {
                namespaces::elaborate_into_current_ns_in(self.arena, env, name)
            } else {
                name
            }
        } else {
            namespaces::elaborate_into_current_ns_in(self.arena, env, name)
        };
        Id(pos, name)
    }
}

#[derive(Clone, Debug)]
enum ClassishNameBuilder<'a> {
    /// We are not in a classish declaration.
    NotInClassish,

    /// We saw a classish keyword token followed by a Name, so we make it
    /// available as the name of the containing class declaration.
    InClassish(&'a (&'a str, &'a Pos<'a>, TokenKind)),
}

impl<'a> ClassishNameBuilder<'a> {
    fn new() -> Self {
        ClassishNameBuilder::NotInClassish
    }

    fn lexed_name_after_classish_keyword(
        &mut self,
        arena: &'a Bump,
        name: &'a str,
        pos: &'a Pos<'a>,
        token_kind: TokenKind,
    ) {
        use ClassishNameBuilder::*;
        match self {
            NotInClassish => {
                let name = if name.starts_with(':') {
                    prefix_slash(arena, name)
                } else {
                    name
                };
                *self = InClassish(arena.alloc((name, pos, token_kind)))
            }
            InClassish(_) => {}
        }
    }

    fn parsed_classish_declaration(&mut self) {
        *self = ClassishNameBuilder::NotInClassish;
    }

    fn get_current_classish_name(&self) -> Option<(&'a str, &'a Pos<'a>)> {
        use ClassishNameBuilder::*;
        match self {
            NotInClassish => None,
            InClassish((name, pos, _)) => Some((name, pos)),
        }
    }

    fn in_interface(&self) -> bool {
        use ClassishNameBuilder::*;
        match self {
            InClassish((_, _, TokenKind::Interface)) => true,
            InClassish((_, _, _)) | NotInClassish => false,
        }
    }
}

#[derive(Debug)]
pub struct FunParamDecl<'a> {
    attributes: Node<'a>,
    visibility: Node<'a>,
    kind: ParamMode,
    readonly: bool,
    hint: Node<'a>,
    pos: &'a Pos<'a>,
    name: Option<&'a str>,
    variadic: bool,
    initializer: Node<'a>,
}

#[derive(Debug)]
pub struct FunctionHeader<'a> {
    name: Node<'a>,
    modifiers: Node<'a>,
    type_params: Node<'a>,
    param_list: Node<'a>,
    capability: Node<'a>,
    ret_hint: Node<'a>,
    readonly_return: Node<'a>,
    where_constraints: Node<'a>,
}

#[derive(Debug)]
pub struct RequireClause<'a> {
    require_type: Node<'a>,
    name: Node<'a>,
}

#[derive(Debug)]
pub struct TypeParameterDecl<'a> {
    name: Node<'a>,
    reified: aast::ReifyKind,
    variance: Variance,
    constraints: &'a [(ConstraintKind, Node<'a>)],
    tparam_params: &'a [&'a Tparam<'a>],
    user_attributes: &'a [&'a UserAttributeNode<'a>],
}

#[derive(Debug)]
pub struct ClosureTypeHint<'a> {
    args: Node<'a>,
    ret_hint: Node<'a>,
}

#[derive(Debug)]
pub struct NamespaceUseClause<'a> {
    kind: NamespaceUseKind,
    id: Id<'a>,
    as_: Option<&'a str>,
}

#[derive(Copy, Clone, Debug)]
enum NamespaceUseKind {
    Type,
    Namespace,
    Mixed,
}

#[derive(Debug)]
pub struct ConstructorNode<'a> {
    method: &'a ShallowMethod<'a>,
    properties: &'a [ShallowProp<'a>],
}

#[derive(Debug)]
pub struct MethodNode<'a> {
    method: &'a ShallowMethod<'a>,
    is_static: bool,
}

#[derive(Debug)]
pub struct PropertyNode<'a> {
    decls: &'a [ShallowProp<'a>],
    is_static: bool,
}

#[derive(Debug)]
pub struct XhpClassAttributeDeclarationNode<'a> {
    xhp_attr_enum_values: &'a [(&'a str, &'a [XhpEnumValue<'a>])],
    xhp_attr_decls: &'a [ShallowProp<'a>],
    xhp_attr_uses_decls: &'a [Node<'a>],
}

#[derive(Debug)]
pub struct XhpClassAttributeNode<'a> {
    name: Id<'a>,
    tag: Option<XhpAttrTag>,
    needs_init: bool,
    nullable: bool,
    hint: Node<'a>,
}

#[derive(Debug)]
pub struct ShapeFieldNode<'a> {
    name: &'a ShapeField<'a>,
    type_: &'a ShapeFieldType<'a>,
}

#[derive(Copy, Clone, Debug)]
struct ClassNameParam<'a> {
    name: Id<'a>,
    full_pos: &'a Pos<'a>, // Position of the full expression `Foo::class`
}

#[derive(Debug)]
pub struct UserAttributeNode<'a> {
    name: Id<'a>,
    classname_params: &'a [ClassNameParam<'a>],
    string_literal_params: &'a [&'a BStr], // this is only used for __Deprecated attribute message and Cipp parameters
}

mod fixed_width_token {
    use parser_core_types::token_kind::TokenKind;
    use std::convert::TryInto;

    #[derive(Copy, Clone)]
    pub struct FixedWidthToken(u64); // { offset: u56, kind: TokenKind }

    const KIND_BITS: u8 = 8;
    const KIND_MASK: u64 = u8::MAX as u64;
    const MAX_OFFSET: u64 = !(KIND_MASK << (64 - KIND_BITS));

    impl FixedWidthToken {
        pub fn new(kind: TokenKind, offset: usize) -> Self {
            // We don't want to spend bits tracking the width of fixed-width
            // tokens. Since we don't track width, verify that this token kind
            // is in fact a fixed-width kind.
            debug_assert!(kind.fixed_width().is_some());

            let offset: u64 = offset.try_into().unwrap();
            if offset > MAX_OFFSET {
                panic!("FixedWidthToken: offset too large: {}", offset);
            }
            Self(offset << KIND_BITS | kind as u8 as u64)
        }

        pub fn offset(self) -> usize {
            (self.0 >> KIND_BITS).try_into().unwrap()
        }

        pub fn kind(self) -> TokenKind {
            TokenKind::try_from_u8(self.0 as u8).unwrap()
        }

        pub fn width(self) -> usize {
            self.kind().fixed_width().unwrap().get()
        }
    }

    impl std::fmt::Debug for FixedWidthToken {
        fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            fmt.debug_struct("FixedWidthToken")
                .field("kind", &self.kind())
                .field("offset", &self.offset())
                .finish()
        }
    }
}
use fixed_width_token::FixedWidthToken;

#[derive(Copy, Clone, Debug)]
pub enum Node<'a> {
    // Nodes which are not useful in constructing a decl are ignored. We keep
    // track of the SyntaxKind for two reasons.
    //
    // One is that the parser needs to know the SyntaxKind of a parsed node in
    // some circumstances (this information is exposed to the parser via an
    // implementation of `smart_constructors::NodeType`). An adapter called
    // WithKind exists to provide a `NodeType` implementation for arbitrary
    // nodes by pairing each node with a SyntaxKind, but in the direct decl
    // parser, we want to avoid the extra 8 bytes of overhead on each node.
    //
    // The second reason is that debugging is difficult when nodes are silently
    // ignored, and providing at least the SyntaxKind of an ignored node helps
    // in tracking down the reason it was ignored.
    Ignored(SyntaxKind),

    List(&'a &'a [Node<'a>]),
    BracketedList(&'a (&'a Pos<'a>, &'a [Node<'a>], &'a Pos<'a>)),
    Name(&'a (&'a str, &'a Pos<'a>)),
    XhpName(&'a (&'a str, &'a Pos<'a>)),
    Variable(&'a (&'a str, &'a Pos<'a>)),
    QualifiedName(&'a (&'a [Node<'a>], &'a Pos<'a>)),
    StringLiteral(&'a (&'a BStr, &'a Pos<'a>)), // For shape keys and const expressions.
    IntLiteral(&'a (&'a str, &'a Pos<'a>)),     // For const expressions.
    FloatingLiteral(&'a (&'a str, &'a Pos<'a>)), // For const expressions.
    BooleanLiteral(&'a (&'a str, &'a Pos<'a>)), // For const expressions.
    Ty(&'a Ty<'a>),
    XhpEnumTy(&'a (&'a Ty<'a>, &'a [XhpEnumValue<'a>])),
    ListItem(&'a (Node<'a>, Node<'a>)),
    Const(&'a ShallowClassConst<'a>), // For the "X=1" in enums "enum E {X=1}" and enum-classes "enum class C {int X=1}", and also for consts via make_const_declaration
    ConstInitializer(&'a (Node<'a>, Node<'a>, &'a [typing_defs::ClassConstRef<'a>])), // Stores (X,1,refs) for "X=1" in top-level "const int X=1" and class-const "public const int X=1".
    FunParam(&'a FunParamDecl<'a>),
    Attribute(&'a UserAttributeNode<'a>),
    FunctionHeader(&'a FunctionHeader<'a>),
    Constructor(&'a ConstructorNode<'a>),
    Method(&'a MethodNode<'a>),
    Property(&'a PropertyNode<'a>),
    EnumUse(&'a Node<'a>),
    TraitUse(&'a Node<'a>),
    XhpClassAttributeDeclaration(&'a XhpClassAttributeDeclarationNode<'a>),
    XhpClassAttribute(&'a XhpClassAttributeNode<'a>),
    XhpAttributeUse(&'a Node<'a>),
    TypeConstant(&'a ShallowTypeconst<'a>),
    ContextConstraint(&'a (ConstraintKind, Node<'a>)),
    RequireClause(&'a RequireClause<'a>),
    ClassishBody(&'a &'a [Node<'a>]),
    TypeParameter(&'a TypeParameterDecl<'a>),
    TypeConstraint(&'a (ConstraintKind, Node<'a>)),
    ShapeFieldSpecifier(&'a ShapeFieldNode<'a>),
    NamespaceUseClause(&'a NamespaceUseClause<'a>),
    Expr(&'a nast::Expr<'a>),
    TypeParameters(&'a &'a [&'a Tparam<'a>]),
    WhereConstraint(&'a WhereConstraint<'a>),
    RecordField(&'a (Id<'a>, RecordFieldReq)),

    // Non-ignored, fixed-width tokens (e.g., keywords, operators, braces, etc.).
    Token(FixedWidthToken),
}

impl<'a> smart_constructors::NodeType for Node<'a> {
    type R = Node<'a>;

    fn extract(self) -> Self::R {
        self
    }

    fn is_abstract(&self) -> bool {
        self.is_token(TokenKind::Abstract)
            || matches!(self, Node::Ignored(SK::Token(TokenKind::Abstract)))
    }
    fn is_name(&self) -> bool {
        matches!(self, Node::Name(..)) || matches!(self, Node::Ignored(SK::Token(TokenKind::Name)))
    }
    fn is_qualified_name(&self) -> bool {
        matches!(self, Node::QualifiedName(..)) || matches!(self, Node::Ignored(SK::QualifiedName))
    }
    fn is_prefix_unary_expression(&self) -> bool {
        matches!(self, Node::Expr(aast::Expr(_, _, aast::Expr_::Unop(..))))
            || matches!(self, Node::Ignored(SK::PrefixUnaryExpression))
    }
    fn is_scope_resolution_expression(&self) -> bool {
        matches!(
            self,
            Node::Expr(aast::Expr(_, _, aast::Expr_::ClassConst(..)))
        ) || matches!(self, Node::Ignored(SK::ScopeResolutionExpression))
    }
    fn is_missing(&self) -> bool {
        matches!(self, Node::Ignored(SK::Missing))
    }
    fn is_variable_expression(&self) -> bool {
        matches!(self, Node::Ignored(SK::VariableExpression))
    }
    fn is_subscript_expression(&self) -> bool {
        matches!(self, Node::Ignored(SK::SubscriptExpression))
    }
    fn is_member_selection_expression(&self) -> bool {
        matches!(self, Node::Ignored(SK::MemberSelectionExpression))
    }
    fn is_object_creation_expression(&self) -> bool {
        matches!(self, Node::Ignored(SK::ObjectCreationExpression))
    }
    fn is_safe_member_selection_expression(&self) -> bool {
        matches!(self, Node::Ignored(SK::SafeMemberSelectionExpression))
    }
    fn is_function_call_expression(&self) -> bool {
        matches!(self, Node::Ignored(SK::FunctionCallExpression))
    }
    fn is_list_expression(&self) -> bool {
        matches!(self, Node::Ignored(SK::ListExpression))
    }
}

impl<'a> Node<'a> {
    fn is_token(self, kind: TokenKind) -> bool {
        self.token_kind() == Some(kind)
    }

    fn token_kind(self) -> Option<TokenKind> {
        match self {
            Node::Token(token) => Some(token.kind()),
            _ => None,
        }
    }

    fn as_slice(self, b: &'a Bump) -> &'a [Self] {
        match self {
            Node::List(&items) | Node::BracketedList(&(_, items, _)) => items,
            n if n.is_ignored() => &[],
            n => std::slice::from_ref(b.alloc(n)),
        }
    }

    fn iter<'b>(&'b self) -> NodeIterHelper<'a, 'b>
    where
        'a: 'b,
    {
        match self {
            &Node::List(&items) | Node::BracketedList(&(_, items, _)) => {
                NodeIterHelper::Vec(items.iter())
            }
            n if n.is_ignored() => NodeIterHelper::Empty,
            n => NodeIterHelper::Single(n),
        }
    }

    // The number of elements which would be yielded by `self.iter()`.
    // Must return the upper bound returned by NodeIterHelper::size_hint.
    fn len(&self) -> usize {
        match self {
            &Node::List(&items) | Node::BracketedList(&(_, items, _)) => items.len(),
            n if n.is_ignored() => 0,
            _ => 1,
        }
    }

    fn as_visibility(&self) -> Option<aast::Visibility> {
        match self.token_kind() {
            Some(TokenKind::Private) => Some(aast::Visibility::Private),
            Some(TokenKind::Protected) => Some(aast::Visibility::Protected),
            Some(TokenKind::Public) => Some(aast::Visibility::Public),
            _ => None,
        }
    }

    // If this node is a simple unqualified identifier, return its position and text.
    fn as_id(&self) -> Option<Id<'a>> {
        match self {
            Node::Name(&(name, pos)) | Node::XhpName(&(name, pos)) => Some(Id(pos, name)),
            _ => None,
        }
    }

    // If this node is a Variable token, return its position and text.
    // As an attempt at error recovery (when the dollar sign is omitted), also
    // return other unqualified identifiers (i.e., the Name token kind).
    fn as_variable(&self) -> Option<Id<'a>> {
        match self {
            Node::Variable(&(name, pos)) | Node::Name(&(name, pos)) => Some(Id(pos, name)),
            _ => None,
        }
    }

    fn is_ignored(&self) -> bool {
        matches!(self, Node::Ignored(..))
    }

    fn is_present(&self) -> bool {
        !self.is_ignored()
    }
}

struct Attributes<'a> {
    deprecated: Option<&'a str>,
    reifiable: Option<&'a Pos<'a>>,
    late_init: bool,
    const_: bool,
    lsb: bool,
    memoizelsb: bool,
    override_: bool,
    enforceable: Option<&'a Pos<'a>>,
    accept_disposable: bool,
    dynamically_callable: bool,
    returns_disposable: bool,
    php_std_lib: bool,
    ifc_attribute: IfcFunDecl<'a>,
    external: bool,
    can_call: bool,
    via_label: bool,
    soft: bool,
    support_dynamic_type: bool,
    module: Option<&'a Module_<'a>>,
    internal: bool,
}

impl<'a, 'text, S: SourceTextAllocator<'text, 'a>> DirectDeclSmartConstructors<'a, 'text, S> {
    fn add_class(&mut self, name: &'a str, decl: &'a shallow_decl_defs::ShallowClass<'a>) {
        self.decls.add(name, Decl::Class(decl), self.arena);
    }
    fn add_fun(&mut self, name: &'a str, decl: &'a typing_defs::FunElt<'a>) {
        self.decls.add(name, Decl::Fun(decl), self.arena);
    }
    fn add_typedef(&mut self, name: &'a str, decl: &'a typing_defs::TypedefType<'a>) {
        self.decls.add(name, Decl::Typedef(decl), self.arena);
    }
    fn add_const(&mut self, name: &'a str, decl: &'a typing_defs::ConstDecl<'a>) {
        self.decls.add(name, Decl::Const(decl), self.arena);
    }
    fn add_record(&mut self, name: &'a str, decl: &'a typing_defs::RecordDefType<'a>) {
        self.decls.add(name, Decl::Record(decl), self.arena);
    }

    #[inline]
    fn concat(&self, str1: &str, str2: &str) -> &'a str {
        concat(self.arena, str1, str2)
    }

    fn token_bytes(&self, token: &CompactToken) -> &'text [u8] {
        self.source_text
            .source_text()
            .sub(token.start_offset(), token.width())
    }

    // Check that the slice is valid UTF-8. If it is, return a &str referencing
    // the same data. Otherwise, copy the slice into our arena using
    // String::from_utf8_lossy_in, and return a reference to the arena str.
    fn str_from_utf8(&self, slice: &'text [u8]) -> &'a str {
        if let Ok(s) = std::str::from_utf8(slice) {
            self.source_text_allocator.alloc(s)
        } else {
            String::from_utf8_lossy_in(slice, self.arena).into_bump_str()
        }
    }

    // Check that the slice is valid UTF-8. If it is, return a &str referencing
    // the same data. Otherwise, copy the slice into our arena using
    // String::from_utf8_lossy_in, and return a reference to the arena str.
    fn str_from_utf8_for_bytes_in_arena(&self, slice: &'a [u8]) -> &'a str {
        if let Ok(s) = std::str::from_utf8(slice) {
            s
        } else {
            String::from_utf8_lossy_in(slice, self.arena).into_bump_str()
        }
    }

    fn merge(
        &self,
        pos1: impl Into<Option<&'a Pos<'a>>>,
        pos2: impl Into<Option<&'a Pos<'a>>>,
    ) -> &'a Pos<'a> {
        match (pos1.into(), pos2.into()) {
            (None, None) => Pos::none(),
            (Some(pos), None) | (None, Some(pos)) => pos,
            (Some(pos1), Some(pos2)) => match (pos1.is_none(), pos2.is_none()) {
                (true, true) => Pos::none(),
                (true, false) => pos2,
                (false, true) => pos1,
                (false, false) => Pos::merge_without_checking_filename(self.arena, pos1, pos2),
            },
        }
    }

    fn merge_positions(&self, node1: Node<'a>, node2: Node<'a>) -> &'a Pos<'a> {
        self.merge(self.get_pos(node1), self.get_pos(node2))
    }

    fn pos_from_slice(&self, nodes: &[Node<'a>]) -> &'a Pos<'a> {
        nodes.iter().fold(Pos::none(), |acc, &node| {
            self.merge(acc, self.get_pos(node))
        })
    }

    fn get_pos(&self, node: Node<'a>) -> &'a Pos<'a> {
        self.get_pos_opt(node).unwrap_or(Pos::none())
    }

    fn get_pos_opt(&self, node: Node<'a>) -> Option<&'a Pos<'a>> {
        let pos = match node {
            Node::Name(&(_, pos)) | Node::Variable(&(_, pos)) => pos,
            Node::Ty(ty) => return ty.get_pos(),
            Node::XhpName(&(_, pos)) => pos,
            Node::QualifiedName(&(_, pos)) => pos,
            Node::IntLiteral(&(_, pos))
            | Node::FloatingLiteral(&(_, pos))
            | Node::StringLiteral(&(_, pos))
            | Node::BooleanLiteral(&(_, pos)) => pos,
            Node::ListItem(&(fst, snd)) => self.merge_positions(fst, snd),
            Node::List(items) => self.pos_from_slice(&items),
            Node::BracketedList(&(first_pos, inner_list, second_pos)) => self.merge(
                first_pos,
                self.merge(self.pos_from_slice(inner_list), second_pos),
            ),
            Node::Expr(&aast::Expr(_, pos, _)) => pos,
            Node::Token(token) => self.token_pos(token),
            _ => return None,
        };
        if pos.is_none() { None } else { Some(pos) }
    }

    fn token_pos(&self, token: FixedWidthToken) -> &'a Pos<'a> {
        let start = token.offset();
        let end = start + token.width();
        let start = self.source_text.offset_to_file_pos_triple(start);
        let end = self.source_text.offset_to_file_pos_triple(end);
        Pos::from_lnum_bol_cnum(self.arena, self.filename, start, end)
    }

    fn node_to_expr(&self, node: Node<'a>) -> Option<&'a nast::Expr<'a>> {
        let expr_ = match node {
            Node::Expr(expr) => return Some(expr),
            Node::IntLiteral(&(s, _)) => aast::Expr_::Int(s),
            Node::FloatingLiteral(&(s, _)) => aast::Expr_::Float(s),
            Node::StringLiteral(&(s, _)) => aast::Expr_::String(s),
            Node::BooleanLiteral((s, _)) => {
                if s.eq_ignore_ascii_case("true") {
                    aast::Expr_::True
                } else {
                    aast::Expr_::False
                }
            }
            Node::Token(t) if t.kind() == TokenKind::NullLiteral => aast::Expr_::Null,
            Node::Name(..) | Node::QualifiedName(..) => {
                aast::Expr_::Id(self.alloc(self.elaborate_const_id(self.expect_name(node)?)))
            }
            _ => return None,
        };
        let pos = self.get_pos(node);
        Some(self.alloc(aast::Expr((), pos, expr_)))
    }

    fn node_to_non_ret_ty(&self, node: Node<'a>) -> Option<&'a Ty<'a>> {
        self.node_to_ty_(node, false)
    }

    fn node_to_ty(&self, node: Node<'a>) -> Option<&'a Ty<'a>> {
        self.node_to_ty_(node, true)
    }

    fn node_to_ty_(&self, node: Node<'a>, allow_non_ret_ty: bool) -> Option<&'a Ty<'a>> {
        match node {
            Node::Ty(Ty(reason, Ty_::Tprim(aast::Tprim::Tvoid))) if !allow_non_ret_ty => {
                Some(self.alloc(Ty(reason, Ty_::Terr)))
            }
            Node::Ty(Ty(reason, Ty_::Tprim(aast::Tprim::Tnoreturn))) if !allow_non_ret_ty => {
                Some(self.alloc(Ty(reason, Ty_::Terr)))
            }
            Node::Ty(ty) => Some(ty),
            Node::Expr(expr) => {
                fn expr_to_ty<'a>(arena: &'a Bump, expr: &'a nast::Expr<'a>) -> Option<Ty_<'a>> {
                    use aast::Expr_::*;
                    match expr.2 {
                        Null => Some(Ty_::Tprim(arena.alloc(aast::Tprim::Tnull))),
                        This => Some(Ty_::Tthis),
                        True | False => Some(Ty_::Tprim(arena.alloc(aast::Tprim::Tbool))),
                        Int(_) => Some(Ty_::Tprim(arena.alloc(aast::Tprim::Tint))),
                        Float(_) => Some(Ty_::Tprim(arena.alloc(aast::Tprim::Tfloat))),
                        String(_) => Some(Ty_::Tprim(arena.alloc(aast::Tprim::Tstring))),
                        String2(_) => Some(Ty_::Tprim(arena.alloc(aast::Tprim::Tstring))),
                        PrefixedString(_) => Some(Ty_::Tprim(arena.alloc(aast::Tprim::Tstring))),
                        Unop(&(_op, expr)) => expr_to_ty(arena, expr),
                        Hole(&(expr, _, _, _)) => expr_to_ty(arena, expr),

                        ArrayGet(_) | As(_) | Await(_) | Binop(_) | Call(_) | Cast(_)
                        | ClassConst(_) | ClassGet(_) | Clone(_) | Collection(_) | Darray(_)
                        | Dollardollar(_) | Efun(_) | Eif(_) | EnumClassLabel(_) | ETSplice(_)
                        | ExpressionTree(_) | FunctionPointer(_) | FunId(_) | Id(_) | Import(_)
                        | Is(_) | KeyValCollection(_) | Lfun(_) | List(_) | Lplaceholder(_)
                        | Lvar(_) | MethodCaller(_) | MethodId(_) | New(_) | ObjGet(_)
                        | Omitted | Pair(_) | Pipe(_) | ReadonlyExpr(_) | Record(_) | Shape(_)
                        | SmethodId(_) | Tuple(_) | Upcast(_) | ValCollection(_) | Varray(_)
                        | Xml(_) | Yield(_) => None,
                    }
                }
                Some(self.alloc(Ty(
                    self.alloc(Reason::witness_from_decl(expr.1)),
                    expr_to_ty(self.arena, expr)?,
                )))
            }
            Node::IntLiteral((_, pos)) => Some(self.alloc(Ty(
                self.alloc(Reason::witness_from_decl(pos)),
                Ty_::Tprim(self.alloc(aast::Tprim::Tint)),
            ))),
            Node::FloatingLiteral((_, pos)) => Some(self.alloc(Ty(
                self.alloc(Reason::witness_from_decl(pos)),
                Ty_::Tprim(self.alloc(aast::Tprim::Tfloat)),
            ))),
            Node::StringLiteral((_, pos)) => Some(self.alloc(Ty(
                self.alloc(Reason::witness_from_decl(pos)),
                Ty_::Tprim(self.alloc(aast::Tprim::Tstring)),
            ))),
            Node::BooleanLiteral((_, pos)) => Some(self.alloc(Ty(
                self.alloc(Reason::witness_from_decl(pos)),
                Ty_::Tprim(self.alloc(aast::Tprim::Tbool)),
            ))),
            Node::Token(t) if t.kind() == TokenKind::Varray => {
                let pos = self.token_pos(t);
                let tany = self.alloc(Ty(self.alloc(Reason::hint(pos)), TANY_));
                let ty_ = Ty_::Tapply(self.alloc((
                    (self.token_pos(t), naming_special_names::collections::VEC),
                    self.alloc([tany]),
                )));
                Some(self.alloc(Ty(self.alloc(Reason::hint(pos)), ty_)))
            }
            Node::Token(t) if t.kind() == TokenKind::Darray => {
                let pos = self.token_pos(t);
                let tany = self.alloc(Ty(self.alloc(Reason::hint(pos)), TANY_));
                let ty_ = Ty_::Tapply(self.alloc((
                    (self.token_pos(t), naming_special_names::collections::DICT),
                    self.alloc([tany, tany]),
                )));
                Some(self.alloc(Ty(self.alloc(Reason::hint(pos)), ty_)))
            }
            Node::Token(t) if t.kind() == TokenKind::This => {
                Some(self.alloc(Ty(self.alloc(Reason::hint(self.token_pos(t))), Ty_::Tthis)))
            }
            Node::Token(t) if t.kind() == TokenKind::NullLiteral => {
                let pos = self.token_pos(t);
                Some(self.alloc(Ty(
                    self.alloc(Reason::hint(pos)),
                    Ty_::Tprim(self.alloc(aast::Tprim::Tnull)),
                )))
            }
            // In coeffects contexts, we get types like `ctx $f` or `$v::C`.
            // Node::Variable is used for the `$f` and `$v`, so that we don't
            // incorrectly attempt to elaborate them as names.
            Node::Variable(&(name, pos)) => Some(self.alloc(Ty(
                self.alloc(Reason::hint(pos)),
                Ty_::Tapply(self.alloc(((pos, name), &[][..]))),
            ))),
            node => {
                let Id(pos, name) = self.expect_name(node)?;
                let reason = self.alloc(Reason::hint(pos));
                let ty_ = if self.is_type_param_in_scope(name) {
                    // TODO (T69662957) must fill type args of Tgeneric
                    Ty_::Tgeneric(self.alloc((name, &[])))
                } else {
                    match name {
                        "nothing" => Ty_::Tunion(&[]),
                        "nonnull" => Ty_::Tnonnull,
                        "dynamic" => Ty_::Tdynamic,
                        "varray_or_darray" | "vec_or_dict" => {
                            let key_type = self.vec_or_dict_key(pos);
                            let value_type = self.alloc(Ty(self.alloc(Reason::hint(pos)), TANY_));
                            Ty_::TvecOrDict(self.alloc((key_type, value_type)))
                        }
                        "_" => Ty_::Terr,
                        _ => {
                            let name = self.elaborate_raw_id(name);
                            Ty_::Tapply(self.alloc(((pos, name), &[][..])))
                        }
                    }
                };
                Some(self.alloc(Ty(reason, ty_)))
            }
        }
    }

    fn to_attributes(&self, node: Node<'a>) -> Attributes<'a> {
        let mut attributes = Attributes {
            deprecated: None,
            reifiable: None,
            late_init: false,
            const_: false,
            lsb: false,
            memoizelsb: false,
            override_: false,
            enforceable: None,
            accept_disposable: false,
            dynamically_callable: false,
            returns_disposable: false,
            php_std_lib: false,
            ifc_attribute: default_ifc_fun_decl(),
            external: false,
            can_call: false,
            via_label: false,
            soft: false,
            support_dynamic_type: false,
            module: None,
            internal: false,
        };

        let nodes = match node {
            Node::List(&nodes) | Node::BracketedList(&(_, nodes, _)) => nodes,
            _ => return attributes,
        };

        let mut ifc_already_policied = false;

        // Iterate in reverse, to match the behavior of OCaml decl in error conditions.
        for attribute in nodes.iter().rev() {
            if let Node::Attribute(attribute) = attribute {
                match attribute.name.1.as_ref() {
                    "__Deprecated" => {
                        attributes.deprecated = attribute
                            .string_literal_params
                            .first()
                            .map(|&x| self.str_from_utf8_for_bytes_in_arena(x));
                    }
                    "__Reifiable" => attributes.reifiable = Some(attribute.name.0),
                    "__LateInit" => {
                        attributes.late_init = true;
                    }
                    "__Const" => {
                        attributes.const_ = true;
                    }
                    "__LSB" => {
                        attributes.lsb = true;
                    }
                    "__MemoizeLSB" => {
                        attributes.memoizelsb = true;
                    }
                    "__Override" => {
                        attributes.override_ = true;
                    }
                    "__Enforceable" => {
                        attributes.enforceable = Some(attribute.name.0);
                    }
                    "__AcceptDisposable" => {
                        attributes.accept_disposable = true;
                    }
                    "__DynamicallyCallable" => {
                        attributes.dynamically_callable = true;
                    }
                    "__ReturnDisposable" => {
                        attributes.returns_disposable = true;
                    }
                    "__PHPStdLib" => {
                        attributes.php_std_lib = true;
                    }
                    "__Policied" => {
                        let string_literal_params = || {
                            attribute
                                .string_literal_params
                                .first()
                                .map(|&x| self.str_from_utf8_for_bytes_in_arena(x))
                        };
                        // Take the classname param by default
                        attributes.ifc_attribute =
                            IfcFunDecl::FDPolicied(attribute.classname_params.first().map_or_else(
                                string_literal_params, // default
                                |&x| Some(x.name.1),   // f
                            ));
                        ifc_already_policied = true;
                    }
                    "__InferFlows" => {
                        if !ifc_already_policied {
                            attributes.ifc_attribute = IfcFunDecl::FDInferFlows;
                        }
                    }
                    "__External" => {
                        attributes.external = true;
                    }
                    "__CanCall" => {
                        attributes.can_call = true;
                    }
                    naming_special_names::user_attributes::VIA_LABEL => {
                        attributes.via_label = true;
                    }
                    "__Soft" => {
                        attributes.soft = true;
                    }
                    "__SupportDynamicType" => {
                        attributes.support_dynamic_type = true;
                    }
                    "__Module" => {
                        attributes.module = attribute
                            .string_literal_params
                            .first()
                            .map(|&x| self.str_from_utf8_for_bytes_in_arena(x))
                            .and_then(|x| {
                                let mut chars = x.split('.');
                                match chars.next() {
                                    None => None,
                                    Some(s) => {
                                        let rest = chars.collect::<std::vec::Vec<_>>();
                                        Some(self.alloc(Module_(s, self.alloc(rest))))
                                    }
                                }
                            });
                    }
                    "__Internal" => {
                        attributes.internal = true;
                    }
                    _ => {}
                }
            }
        }

        attributes
    }

    // Limited version of node_to_ty that matches behavior of Decl_utils.infer_const
    fn infer_const(&self, name: Node<'a>, node: Node<'a>) -> Option<&'a Ty<'a>> {
        match node {
            Node::StringLiteral(_)
            | Node::BooleanLiteral(_)
            | Node::IntLiteral(_)
            | Node::FloatingLiteral(_)
            | Node::Expr(aast::Expr(_, _, aast::Expr_::Unop(&(Uop::Uminus, _))))
            | Node::Expr(aast::Expr(_, _, aast::Expr_::Unop(&(Uop::Uplus, _))))
            | Node::Expr(aast::Expr(_, _, aast::Expr_::String(..))) => self.node_to_ty(node),
            Node::Token(t) if t.kind() == TokenKind::NullLiteral => {
                let pos = self.token_pos(t);
                Some(self.alloc(Ty(
                    self.alloc(Reason::witness_from_decl(pos)),
                    Ty_::Tprim(self.alloc(aast::Tprim::Tnull)),
                )))
            }
            _ => Some(self.tany_with_pos(self.get_pos(name))),
        }
    }

    fn pop_type_params(&mut self, node: Node<'a>) -> &'a [&'a Tparam<'a>] {
        match node {
            Node::TypeParameters(tparams) => {
                Rc::make_mut(&mut self.type_parameters).pop().unwrap();
                tparams
            }
            _ => &[],
        }
    }

    fn ret_from_fun_kind(&self, kind: FunKind, type_: &'a Ty<'a>) -> &'a Ty<'a> {
        let pos = type_.get_pos().unwrap_or_else(|| Pos::none());
        match kind {
            FunKind::FAsyncGenerator => self.alloc(Ty(
                self.alloc(Reason::RretFunKindFromDecl(self.alloc((pos, kind)))),
                Ty_::Tapply(self.alloc((
                    (pos, naming_special_names::classes::ASYNC_GENERATOR),
                    self.alloc([type_, type_, type_]),
                ))),
            )),
            FunKind::FGenerator => self.alloc(Ty(
                self.alloc(Reason::RretFunKindFromDecl(self.alloc((pos, kind)))),
                Ty_::Tapply(self.alloc((
                    (pos, naming_special_names::classes::GENERATOR),
                    self.alloc([type_, type_, type_]),
                ))),
            )),
            FunKind::FAsync => self.alloc(Ty(
                self.alloc(Reason::RretFunKindFromDecl(self.alloc((pos, kind)))),
                Ty_::Tapply(self.alloc((
                    (pos, naming_special_names::classes::AWAITABLE),
                    self.alloc([type_]),
                ))),
            )),
            _ => type_,
        }
    }

    fn is_type_param_in_scope(&self, name: &str) -> bool {
        self.type_parameters.iter().any(|tps| tps.contains(name))
    }

    fn as_fun_implicit_params(
        &mut self,
        capability: Node<'a>,
        default_pos: &'a Pos<'a>,
    ) -> &'a FunImplicitParams<'a> {
        /* Note: do not simplify intersections, keep empty / singleton intersections
         * for coeffect contexts
         */
        let capability = match self.node_to_ty(capability) {
            Some(ty) => CapTy(ty),
            None => CapDefaults(default_pos),
        };
        self.alloc(FunImplicitParams { capability })
    }

    fn function_to_ty(
        &mut self,
        is_method: bool,
        attributes: Node<'a>,
        header: &'a FunctionHeader<'a>,
        body: Node<'_>,
    ) -> Option<(PosId<'a>, &'a Ty<'a>, &'a [ShallowProp<'a>])> {
        let id_opt = match (is_method, header.name) {
            (true, Node::Token(t)) if t.kind() == TokenKind::Construct => {
                let pos = self.token_pos(t);
                Some(Id(pos, naming_special_names::members::__CONSTRUCT))
            }
            (true, _) => self.expect_name(header.name),
            (false, _) => self.elaborate_defined_id(header.name),
        };
        let id = id_opt.unwrap_or(Id(self.get_pos(header.name), ""));
        let (params, properties, arity) = self.as_fun_params(header.param_list)?;
        let f_pos = self.get_pos(header.name);
        let implicit_params = self.as_fun_implicit_params(header.capability, f_pos);

        let type_ = match header.name {
            Node::Token(t) if t.kind() == TokenKind::Construct => {
                let pos = self.token_pos(t);
                self.alloc(Ty(
                    self.alloc(Reason::witness_from_decl(pos)),
                    Ty_::Tprim(self.alloc(aast::Tprim::Tvoid)),
                ))
            }
            _ => self
                .node_to_ty(header.ret_hint)
                .unwrap_or_else(|| self.tany_with_pos(f_pos)),
        };
        let async_ = header
            .modifiers
            .iter()
            .any(|n| n.is_token(TokenKind::Async));
        let readonly = header
            .modifiers
            .iter()
            .any(|n| n.is_token(TokenKind::Readonly));

        let fun_kind = if body.iter().any(|node| node.is_token(TokenKind::Yield)) {
            if async_ {
                FunKind::FAsyncGenerator
            } else {
                FunKind::FGenerator
            }
        } else {
            if async_ {
                FunKind::FAsync
            } else {
                FunKind::FSync
            }
        };
        let type_ = if !header.ret_hint.is_present() {
            self.ret_from_fun_kind(fun_kind, type_)
        } else {
            type_
        };
        let attributes = self.to_attributes(attributes);
        // TODO(hrust) Put this in a helper. Possibly do this for all flags.
        let mut flags = match fun_kind {
            FunKind::FSync => FunTypeFlags::empty(),
            FunKind::FAsync => FunTypeFlags::ASYNC,
            FunKind::FGenerator => FunTypeFlags::GENERATOR,
            FunKind::FAsyncGenerator => FunTypeFlags::ASYNC | FunTypeFlags::GENERATOR,
        };

        if attributes.returns_disposable {
            flags |= FunTypeFlags::RETURN_DISPOSABLE;
        }
        if header.readonly_return.is_token(TokenKind::Readonly) {
            flags |= FunTypeFlags::RETURNS_READONLY;
        }
        if readonly {
            flags |= FunTypeFlags::READONLY_THIS
        }

        let ifc_decl = attributes.ifc_attribute;

        // Pop the type params stack only after creating all inner types.
        let tparams = self.pop_type_params(header.type_params);

        let where_constraints =
            self.slice(header.where_constraints.iter().filter_map(|&x| match x {
                Node::WhereConstraint(x) => Some(x),
                _ => None,
            }));

        let (params, tparams, implicit_params, where_constraints) =
            self.rewrite_effect_polymorphism(params, tparams, implicit_params, where_constraints);

        let ft = self.alloc(FunType {
            arity,
            tparams,
            where_constraints,
            params,
            implicit_params,
            ret: self.alloc(PossiblyEnforcedTy {
                enforced: Enforcement::Unenforced,
                type_,
            }),
            flags,
            ifc_decl,
        });

        let ty = self.alloc(Ty(
            self.alloc(Reason::witness_from_decl(id.0)),
            Ty_::Tfun(ft),
        ));
        Some((id.into(), ty, properties))
    }

    fn as_fun_params(
        &self,
        list: Node<'a>,
    ) -> Option<(&'a FunParams<'a>, &'a [ShallowProp<'a>], FunArity<'a>)> {
        match list {
            Node::List(nodes) => {
                let mut params = Vec::with_capacity_in(nodes.len(), self.arena);
                let mut properties = Vec::new_in(self.arena);
                let mut arity = FunArity::Fstandard;
                for node in nodes.iter() {
                    match node {
                        Node::FunParam(&FunParamDecl {
                            attributes,
                            visibility,
                            kind,
                            readonly,
                            hint,
                            pos,
                            name,
                            variadic,
                            initializer,
                        }) => {
                            let attributes = self.to_attributes(attributes);

                            if let Some(visibility) = visibility.as_visibility() {
                                let name = name.unwrap_or("");
                                let name = strip_dollar_prefix(name);
                                let mut flags = PropFlags::empty();
                                flags.set(PropFlags::CONST, attributes.const_);
                                flags.set(PropFlags::NEEDS_INIT, self.file_mode != Mode::Mhhi);
                                flags.set(PropFlags::PHP_STD_LIB, attributes.php_std_lib);
                                flags.set(PropFlags::READONLY, readonly);
                                properties.push(ShallowProp {
                                    xhp_attr: None,
                                    name: (pos, name),
                                    type_: self.node_to_ty(hint),
                                    visibility,
                                    flags,
                                });
                            }

                            let type_ = if hint.is_ignored() {
                                self.tany_with_pos(pos)
                            } else {
                                self.node_to_ty(hint)?
                            };
                            // These are illegal here--they can only be used on
                            // parameters in a function type hint (see
                            // make_closure_type_specifier and unwrap_mutability).
                            // Unwrap them here anyway for better error recovery.
                            let type_ = match type_ {
                                Ty(_, Ty_::Tapply(((_, "\\Mutable"), [t]))) => t,
                                Ty(_, Ty_::Tapply(((_, "\\OwnedMutable"), [t]))) => t,
                                Ty(_, Ty_::Tapply(((_, "\\MaybeMutable"), [t]))) => t,
                                _ => type_,
                            };
                            let mut flags = FunParamFlags::empty();
                            if attributes.accept_disposable {
                                flags |= FunParamFlags::ACCEPT_DISPOSABLE
                            }
                            if attributes.external {
                                flags |= FunParamFlags::IFC_EXTERNAL
                            }
                            if attributes.can_call {
                                flags |= FunParamFlags::IFC_CAN_CALL
                            }
                            if attributes.via_label {
                                flags |= FunParamFlags::VIA_LABEL
                            }
                            if readonly {
                                flags |= FunParamFlags::READONLY
                            }
                            match kind {
                                ParamMode::FPinout => {
                                    flags |= FunParamFlags::INOUT;
                                }
                                ParamMode::FPnormal => {}
                            };

                            if initializer.is_present() {
                                flags |= FunParamFlags::HAS_DEFAULT;
                            }
                            let variadic = initializer.is_ignored() && variadic;
                            let type_ = if variadic {
                                self.alloc(Ty(
                                    self.alloc(if name.is_some() {
                                        Reason::RvarParamFromDecl(pos)
                                    } else {
                                        Reason::witness_from_decl(pos)
                                    }),
                                    type_.1,
                                ))
                            } else {
                                type_
                            };
                            let param = self.alloc(FunParam {
                                pos,
                                name,
                                type_: self.alloc(PossiblyEnforcedTy {
                                    enforced: Enforcement::Unenforced,
                                    type_,
                                }),
                                flags,
                            });
                            arity = match arity {
                                FunArity::Fstandard if variadic => FunArity::Fvariadic(param),
                                arity => {
                                    params.push(param);
                                    arity
                                }
                            };
                        }
                        _ => {}
                    }
                }
                Some((
                    params.into_bump_slice(),
                    properties.into_bump_slice(),
                    arity,
                ))
            }
            n if n.is_ignored() => Some((&[], &[], FunArity::Fstandard)),
            _ => None,
        }
    }

    fn make_shape_field_name(&self, name: Node<'a>) -> Option<ShapeFieldName<'a>> {
        Some(match name {
            Node::StringLiteral(&(s, pos)) => ShapeFieldName::SFlitStr(self.alloc((pos, s))),
            // TODO: OCaml decl produces SFlitStr here instead of SFlitInt, so
            // we must also. Looks like int literal keys have become a parse
            // error--perhaps that's why.
            Node::IntLiteral(&(s, pos)) => ShapeFieldName::SFlitStr(self.alloc((pos, s.into()))),
            Node::Expr(aast::Expr(
                _,
                _,
                aast::Expr_::ClassConst(&(
                    aast::ClassId(_, _, aast::ClassId_::CI(&class_name)),
                    const_name,
                )),
            )) => ShapeFieldName::SFclassConst(self.alloc((class_name, const_name))),
            Node::Expr(aast::Expr(
                _,
                _,
                aast::Expr_::ClassConst(&(
                    aast::ClassId(_, pos, aast::ClassId_::CIself),
                    const_name,
                )),
            )) => ShapeFieldName::SFclassConst(self.alloc((
                Id(
                    pos,
                    self.classish_name_builder.get_current_classish_name()?.0,
                ),
                const_name,
            ))),
            _ => return None,
        })
    }



    fn make_t_shape_field_name(&mut self, ShapeField(field): &ShapeField<'a>) -> TShapeField<'a> {
        TShapeField(match field {
            ShapeFieldName::SFlitInt(&(pos, x)) => {
                TshapeFieldName::TSFlitInt(self.alloc(PosString(pos, x)))
            }
            ShapeFieldName::SFlitStr(&(pos, x)) => {
                TshapeFieldName::TSFlitStr(self.alloc(PosByteString(pos, x)))
            }
            ShapeFieldName::SFclassConst(&(id, &(pos, x))) => {
                TshapeFieldName::TSFclassConst(self.alloc((id.into(), PosString(pos, x))))
            }
        })
    }

    fn make_apply(
        &self,
        base_ty: PosId<'a>,
        type_arguments: Node<'a>,
        pos_to_merge: &'a Pos<'a>,
    ) -> Node<'a> {
        let type_arguments = self.slice(
            type_arguments
                .iter()
                .filter_map(|&node| self.node_to_ty(node)),
        );

        let pos = self.merge(base_ty.0, pos_to_merge);

        // OCaml decl creates a capability with a hint pointing to the entire
        // type (i.e., pointing to `Rx<(function(): void)>` rather than just
        // `(function(): void)`), so we extend the hint position similarly here.
        let extend_capability_pos = |implicit_params: &'a FunImplicitParams<'_>| {
            let capability = match implicit_params.capability {
                CapTy(ty) => {
                    let ty = self.alloc(Ty(self.alloc(Reason::hint(pos)), ty.1));
                    CapTy(ty)
                }
                CapDefaults(_) => CapDefaults(pos),
            };
            self.alloc(FunImplicitParams {
                capability,
                ..*implicit_params
            })
        };

        let ty_ = match (base_ty, type_arguments) {
            ((_, name), &[&Ty(_, Ty_::Tfun(f))]) if name == "\\Pure" => {
                Ty_::Tfun(self.alloc(FunType {
                    implicit_params: extend_capability_pos(f.implicit_params),
                    ..*f
                }))
            }
            _ => Ty_::Tapply(self.alloc((base_ty, type_arguments))),
        };

        self.hint_ty(pos, ty_)
    }

    fn hint_ty(&self, pos: &'a Pos<'a>, ty_: Ty_<'a>) -> Node<'a> {
        Node::Ty(self.alloc(Ty(self.alloc(Reason::hint(pos)), ty_)))
    }

    fn prim_ty(&self, tprim: aast::Tprim, pos: &'a Pos<'a>) -> Node<'a> {
        self.hint_ty(pos, Ty_::Tprim(self.alloc(tprim)))
    }

    fn tany_with_pos(&self, pos: &'a Pos<'a>) -> &'a Ty<'a> {
        self.alloc(Ty(self.alloc(Reason::witness_from_decl(pos)), TANY_))
    }

    /// The type used when a `vec_or_dict` typehint is missing its key type argument.
    fn vec_or_dict_key(&self, pos: &'a Pos<'a>) -> &'a Ty<'a> {
        self.alloc(Ty(
            self.alloc(Reason::RvecOrDictKey(pos)),
            Ty_::Tprim(self.alloc(aast::Tprim::Tarraykey)),
        ))
    }

    fn source_text_at_pos(&self, pos: &'a Pos<'a>) -> &'text [u8] {
        let start = pos.start_cnum();
        let end = pos.end_cnum();
        self.source_text.source_text().sub(start, end - start)
    }

    // While we usually can tell whether to allocate a Tapply or Tgeneric based
    // on our type_parameters stack, *constraints* on type parameters may
    // reference type parameters which we have not parsed yet. When constructing
    // a type parameter list, we use this function to rewrite the type of each
    // constraint, considering the full list of type parameters to be in scope.
    fn convert_tapply_to_tgeneric(&self, ty: &'a Ty<'a>) -> &'a Ty<'a> {
        let ty_ = match ty.1 {
            Ty_::Tapply(&(id, targs)) => {
                let converted_targs = self.slice(
                    targs
                        .iter()
                        .map(|&targ| self.convert_tapply_to_tgeneric(targ)),
                );
                match self.tapply_should_be_tgeneric(ty.0, id) {
                    Some(name) => Ty_::Tgeneric(self.alloc((name, converted_targs))),
                    None => Ty_::Tapply(self.alloc((id, converted_targs))),
                }
            }
            Ty_::Tlike(ty) => Ty_::Tlike(self.convert_tapply_to_tgeneric(ty)),
            Ty_::Toption(ty) => Ty_::Toption(self.convert_tapply_to_tgeneric(ty)),
            Ty_::Tfun(fun_type) => {
                let convert_param = |param: &'a FunParam<'a>| {
                    self.alloc(FunParam {
                        type_: self.alloc(PossiblyEnforcedTy {
                            enforced: param.type_.enforced,
                            type_: self.convert_tapply_to_tgeneric(param.type_.type_),
                        }),
                        ..*param
                    })
                };
                let arity = match fun_type.arity {
                    FunArity::Fstandard => FunArity::Fstandard,
                    FunArity::Fvariadic(param) => FunArity::Fvariadic(convert_param(param)),
                };
                let params = self.slice(fun_type.params.iter().copied().map(convert_param));
                let implicit_params = fun_type.implicit_params;
                let ret = self.alloc(PossiblyEnforcedTy {
                    enforced: fun_type.ret.enforced,
                    type_: self.convert_tapply_to_tgeneric(fun_type.ret.type_),
                });
                Ty_::Tfun(self.alloc(FunType {
                    arity,
                    params,
                    implicit_params,
                    ret,
                    ..*fun_type
                }))
            }
            Ty_::Tshape(&(kind, fields)) => {
                let mut converted_fields = AssocListMut::with_capacity_in(fields.len(), self.arena);
                for (&name, ty) in fields.iter() {
                    converted_fields.insert(
                        name,
                        self.alloc(ShapeFieldType {
                            optional: ty.optional,
                            ty: self.convert_tapply_to_tgeneric(ty.ty),
                        }),
                    );
                }
                Ty_::Tshape(self.alloc((kind, converted_fields.into())))
            }
            Ty_::Tdarray(&(tk, tv)) => Ty_::Tdarray(self.alloc((
                self.convert_tapply_to_tgeneric(tk),
                self.convert_tapply_to_tgeneric(tv),
            ))),
            Ty_::Tvarray(ty) => Ty_::Tvarray(self.convert_tapply_to_tgeneric(ty)),
            Ty_::TvarrayOrDarray(&(tk, tv)) => Ty_::TvarrayOrDarray(self.alloc((
                self.convert_tapply_to_tgeneric(tk),
                self.convert_tapply_to_tgeneric(tv),
            ))),
            Ty_::TvecOrDict(&(tk, tv)) => Ty_::TvecOrDict(self.alloc((
                self.convert_tapply_to_tgeneric(tk),
                self.convert_tapply_to_tgeneric(tv),
            ))),
            Ty_::Ttuple(tys) => Ty_::Ttuple(
                self.slice(
                    tys.iter()
                        .map(|&targ| self.convert_tapply_to_tgeneric(targ)),
                ),
            ),
            _ => return ty,
        };
        self.alloc(Ty(ty.0, ty_))
    }

    // This is the logic for determining if convert_tapply_to_tgeneric should turn
    // a Tapply into a Tgeneric
    fn tapply_should_be_tgeneric(&self, reason: &'a Reason<'a>, id: PosId<'a>) -> Option<&'a str> {
        match reason.pos() {
            // If the name contained a namespace delimiter in the original
            // source text, then it can't have referred to a type parameter
            // (since type parameters cannot be namespaced).
            Some(pos) => {
                if self.source_text_at_pos(pos).contains(&b'\\') {
                    return None;
                }
            }
            None => return None,
        }
        // However, the direct decl parser will unconditionally prefix
        // the name with the current namespace (as it does for any
        // Tapply). We need to remove it.
        match id.1.rsplit('\\').next() {
            Some(name) if self.is_type_param_in_scope(name) => return Some(name),
            _ => return None,
        }
    }

    fn rewrite_taccess_reasons(&self, ty: &'a Ty<'a>, r: &'a Reason<'a>) -> &'a Ty<'a> {
        let ty_ = match ty.1 {
            Ty_::Taccess(&TaccessType(ty, id)) => {
                Ty_::Taccess(self.alloc(TaccessType(self.rewrite_taccess_reasons(ty, r), id)))
            }
            ty_ => ty_,
        };
        self.alloc(Ty(r, ty_))
    }

    fn user_attribute_to_decl(
        &self,
        attr: &UserAttributeNode<'a>,
    ) -> &'a shallow_decl_defs::UserAttribute<'a> {
        self.alloc(shallow_decl_defs::UserAttribute {
            name: attr.name.into(),
            classname_params: self.slice(attr.classname_params.iter().map(|p| p.name.1)),
        })
    }

    fn namespace_use_kind(use_kind: &Node<'_>) -> Option<NamespaceUseKind> {
        match use_kind.token_kind() {
            Some(TokenKind::Const) => None,
            Some(TokenKind::Function) => None,
            Some(TokenKind::Type) => Some(NamespaceUseKind::Type),
            Some(TokenKind::Namespace) => Some(NamespaceUseKind::Namespace),
            _ if !use_kind.is_present() => Some(NamespaceUseKind::Mixed),
            _ => None,
        }
    }

    fn has_polymorphic_context(contexts: &[&Ty<'_>]) -> bool {
        contexts.iter().any(|&ty| match ty.1 {
            Ty_::Tapply((root, &[])) // Hfun_context in the AST
            | Ty_::Taccess(TaccessType(Ty(_, Ty_::Tapply((root, &[]))), _)) => root.1.contains('$'),
            | Ty_::Taccess(TaccessType(t, _)) => Self::taccess_root_is_generic(t),
            _ => false,
        })
    }

    fn ctx_generic_for_fun(&self, name: &str) -> &'a str {
        bumpalo::format!(in self.arena, "T/[ctx {}]", name).into_bump_str()
    }

    fn ctx_generic_for_dependent(&self, name: &str, cst: &str) -> &'a str {
        bumpalo::format!(in self.arena, "T/[{}::{}]", name, cst).into_bump_str()
    }

    // Note: the reason for the divergence between this and the lowerer is that
    // hint Haccess is a flat list, whereas decl ty Taccess is a tree.
    fn taccess_root_is_generic(ty: &Ty<'_>) -> bool {
        match ty {
            Ty(_, Ty_::Tgeneric((_, &[]))) => true,
            Ty(_, Ty_::Taccess(&TaccessType(t, _))) => Self::taccess_root_is_generic(t),
            _ => false,
        }
    }

    fn ctx_generic_for_generic_taccess_inner(&self, ty: &Ty<'_>, cst: &str) -> std::string::String {
        let left = match ty {
            Ty(_, Ty_::Tgeneric((name, &[]))) => name.to_string(),
            Ty(_, Ty_::Taccess(&TaccessType(ty, cst))) => {
                self.ctx_generic_for_generic_taccess_inner(ty, cst.1)
            }
            _ => panic!("Unexpected element in Taccess"),
        };
        format!("{}::{}", left, cst)
    }
    fn ctx_generic_for_generic_taccess(&self, ty: &Ty<'_>, cst: &str) -> &'a str {
        bumpalo::format!(in self.arena, "T/[{}]", self.ctx_generic_for_generic_taccess_inner(ty, cst))
            .into_bump_str()
    }

    fn rewrite_effect_polymorphism(
        &self,
        params: &'a [&'a FunParam<'a>],
        tparams: &'a [&'a Tparam<'a>],
        implicit_params: &'a FunImplicitParams<'a>,
        where_constraints: &'a [&'a WhereConstraint<'a>],
    ) -> (
        &'a [&'a FunParam<'a>],
        &'a [&'a Tparam<'a>],
        &'a FunImplicitParams<'a>,
        &'a [&'a WhereConstraint<'a>],
    ) {
        let (cap_reason, context_tys) = match implicit_params.capability {
            CapTy(&Ty(r, Ty_::Tintersection(tys))) if Self::has_polymorphic_context(tys) => {
                (r, tys)
            }
            CapTy(ty) if Self::has_polymorphic_context(&[ty]) => {
                (ty.0, std::slice::from_ref(self.alloc(ty)))
            }
            _ => return (params, tparams, implicit_params, where_constraints),
        };
        let tp = |name, constraints| {
            self.alloc(Tparam {
                variance: Variance::Invariant,
                name,
                tparams: &[],
                constraints,
                reified: aast::ReifyKind::Erased,
                user_attributes: &[],
            })
        };

        // For a polymorphic context with form `ctx $f` (represented here as
        // `Tapply "$f"`), add a type parameter named `Tctx$f`, and rewrite the
        // parameter `(function (ts)[_]: t) $f` as `(function (ts)[Tctx$f]: t) $f`
        let rewrite_fun_ctx =
            |tparams: &mut Vec<'_, &'a Tparam<'a>>, ty: &Ty<'a>, param_name: &str| -> Ty<'a> {
                let ft = match ty.1 {
                    Ty_::Tfun(ft) => ft,
                    _ => return ty.clone(),
                };
                let cap_ty = match ft.implicit_params.capability {
                    CapTy(&Ty(_, Ty_::Tintersection(&[ty]))) | CapTy(ty) => ty,
                    _ => return ty.clone(),
                };
                let pos = match cap_ty.1 {
                    Ty_::Tapply(((pos, "_"), _)) => pos,
                    _ => return ty.clone(),
                };
                let name = self.ctx_generic_for_fun(param_name);
                let tparam = tp((pos, name), &[]);
                tparams.push(tparam);
                let cap_ty = self.alloc(Ty(cap_ty.0, Ty_::Tgeneric(self.alloc((name, &[])))));
                let ft = self.alloc(FunType {
                    implicit_params: self.alloc(FunImplicitParams {
                        capability: CapTy(cap_ty),
                    }),
                    ..*ft
                });
                Ty(ty.0, Ty_::Tfun(ft))
            };

        // For a polymorphic context with form `$g::C`, if we have a function
        // parameter `$g` with type `G` (where `G` is not a type parameter),
        //   - add a type parameter constrained by $g's type: `T/$g as G`
        //   - replace $g's type hint (`G`) with the new type parameter `T/$g`
        // Then, for each polymorphic context with form `$g::C`,
        //   - add a type parameter `T/[$g::C]`
        //   - add a where constraint `T/[$g::C] = T$g :: C`
        let rewrite_arg_ctx = |
            tparams: &mut Vec<'_, &'a Tparam<'a>>,
            where_constraints: &mut Vec<'_, &'a WhereConstraint<'a>>,
            ty: &Ty<'a>,
            param_pos: &'a Pos<'a>,
            name: &str,
            context_reason: &'a Reason<'a>,
            cst: PosId<'a>,
        | -> Ty<'a> {
            let rewritten_ty = match ty.1 {
                // If the type hint for this function parameter is a type
                // parameter introduced in this function declaration, don't add
                // a new type parameter.
                Ty_::Tgeneric(&(type_name, _))
                    if tparams.iter().any(|tp| tp.name.1 == type_name) =>
                {
                    ty.clone()
                }
                // Otherwise, if the parameter is `G $g`, create tparam
                // `T$g as G` and replace $g's type hint
                _ => {
                    let id = (param_pos, self.concat("T/", name));
                    tparams.push(tp(
                        id,
                        std::slice::from_ref(
                            self.alloc((ConstraintKind::ConstraintAs, self.alloc(ty.clone()))),
                        ),
                    ));
                    Ty(
                        self.alloc(Reason::hint(param_pos)),
                        Ty_::Tgeneric(self.alloc((id.1, &[]))),
                    )
                }
            };
            let ty = self.alloc(Ty(context_reason, rewritten_ty.1));
            let right = self.alloc(Ty(
                context_reason,
                Ty_::Taccess(self.alloc(TaccessType(ty, cst))),
            ));
            let left_id = (
                context_reason.pos().unwrap_or(Pos::none()),
                self.ctx_generic_for_dependent(name, &cst.1),
            );
            tparams.push(tp(left_id, &[]));
            let left = self.alloc(Ty(
                context_reason,
                Ty_::Tgeneric(self.alloc((left_id.1, &[]))),
            ));
            where_constraints.push(self.alloc(WhereConstraint(
                left,
                ConstraintKind::ConstraintEq,
                right,
            )));
            rewritten_ty
        };

        let mut tparams = Vec::from_iter_in(tparams.iter().copied(), self.arena);
        let mut where_constraints =
            Vec::from_iter_in(where_constraints.iter().copied(), self.arena);

        // The divergence here from the lowerer comes from using oxidized_by_ref instead of oxidized
        let mut ty_by_param: BTreeMap<&str, (Ty<'a>, &'a Pos<'a>)> = params
            .iter()
            .filter_map(|param| Some((param.name?, (param.type_.type_.clone(), param.pos))))
            .collect();

        for context_ty in context_tys {
            match context_ty.1 {
                // Hfun_context in the AST.
                Ty_::Tapply(((_, name), _)) if name.starts_with('$') => {
                    if let Some((param_ty, _)) = ty_by_param.get_mut(name) {
                        match param_ty.1 {
                            Ty_::Tlike(ref mut ty) => match ty {
                                Ty(r, Ty_::Toption(tinner)) => {
                                    *ty = self.alloc(Ty(
                                        r,
                                        Ty_::Toption(self.alloc(rewrite_fun_ctx(
                                            &mut tparams,
                                            tinner,
                                            name,
                                        ))),
                                    ))
                                }
                                _ => {
                                    *ty = self.alloc(rewrite_fun_ctx(&mut tparams, ty, name));
                                }
                            },
                            Ty_::Toption(ref mut ty) => {
                                *ty = self.alloc(rewrite_fun_ctx(&mut tparams, ty, name));
                            }
                            _ => {
                                *param_ty = rewrite_fun_ctx(&mut tparams, param_ty, name);
                            }
                        }
                    }
                }
                Ty_::Taccess(&TaccessType(Ty(_, Ty_::Tapply(((_, name), _))), cst)) => {
                    if let Some((param_ty, param_pos)) = ty_by_param.get_mut(name) {
                        let mut rewrite = |t| {
                            rewrite_arg_ctx(
                                &mut tparams,
                                &mut where_constraints,
                                t,
                                param_pos,
                                name,
                                context_ty.0,
                                cst,
                            )
                        };
                        match param_ty.1 {
                            Ty_::Tlike(ref mut ty) => match ty {
                                Ty(r, Ty_::Toption(tinner)) => {
                                    *ty =
                                        self.alloc(Ty(r, Ty_::Toption(self.alloc(rewrite(tinner)))))
                                }
                                _ => {
                                    *ty = self.alloc(rewrite(ty));
                                }
                            },
                            Ty_::Toption(ref mut ty) => {
                                *ty = self.alloc(rewrite(ty));
                            }
                            _ => {
                                *param_ty = rewrite(param_ty);
                            }
                        }
                    }
                }
                Ty_::Taccess(&TaccessType(t, cst)) if Self::taccess_root_is_generic(t) => {
                    let left_id = (
                        context_ty.0.pos().unwrap_or(Pos::none()),
                        self.ctx_generic_for_generic_taccess(t, &cst.1),
                    );
                    tparams.push(tp(left_id, &[]));
                    let left = self.alloc(Ty(
                        context_ty.0,
                        Ty_::Tgeneric(self.alloc((left_id.1, &[]))),
                    ));
                    where_constraints.push(self.alloc(WhereConstraint(
                        left,
                        ConstraintKind::ConstraintEq,
                        context_ty,
                    )));
                }
                _ => {}
            }
        }

        let params = self.slice(params.iter().copied().map(|param| match param.name {
            None => param,
            Some(name) => match ty_by_param.get(name) {
                Some((type_, _)) if param.type_.type_ != type_ => self.alloc(FunParam {
                    type_: self.alloc(PossiblyEnforcedTy {
                        type_: self.alloc(type_.clone()),
                        ..*param.type_
                    }),
                    ..*param
                }),
                _ => param,
            },
        }));

        let context_tys = self.slice(context_tys.iter().copied().map(|ty| {
            let ty_ = match ty.1 {
                Ty_::Tapply(((_, name), &[])) if name.starts_with('$') => {
                    Ty_::Tgeneric(self.alloc((self.ctx_generic_for_fun(name), &[])))
                }
                Ty_::Taccess(&TaccessType(Ty(_, Ty_::Tapply(((_, name), &[]))), cst))
                    if name.starts_with('$') =>
                {
                    let name = self.ctx_generic_for_dependent(name, &cst.1);
                    Ty_::Tgeneric(self.alloc((name, &[])))
                }
                Ty_::Taccess(&TaccessType(t, cst)) if Self::taccess_root_is_generic(t) => {
                    let name = self.ctx_generic_for_generic_taccess(t, &cst.1);
                    Ty_::Tgeneric(self.alloc((name, &[])))
                }
                _ => return ty,
            };
            self.alloc(Ty(ty.0, ty_))
        }));
        let cap_ty = match context_tys {
            [ty] => ty,
            _ => self.alloc(Ty(cap_reason, Ty_::Tintersection(context_tys))),
        };
        let implicit_params = self.alloc(FunImplicitParams {
            capability: CapTy(cap_ty),
        });

        (
            params,
            tparams.into_bump_slice(),
            implicit_params,
            where_constraints.into_bump_slice(),
        )
    }
}

enum NodeIterHelper<'a, 'b> {
    Empty,
    Single(&'b Node<'a>),
    Vec(std::slice::Iter<'b, Node<'a>>),
}

impl<'a, 'b> Iterator for NodeIterHelper<'a, 'b> {
    type Item = &'b Node<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            NodeIterHelper::Empty => None,
            NodeIterHelper::Single(node) => {
                let node = *node;
                *self = NodeIterHelper::Empty;
                Some(node)
            }
            NodeIterHelper::Vec(ref mut iter) => iter.next(),
        }
    }

    // Must return the upper bound returned by Node::len.
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            NodeIterHelper::Empty => (0, Some(0)),
            NodeIterHelper::Single(_) => (1, Some(1)),
            NodeIterHelper::Vec(iter) => iter.size_hint(),
        }
    }
}

impl<'a, 'b> DoubleEndedIterator for NodeIterHelper<'a, 'b> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self {
            NodeIterHelper::Empty => None,
            NodeIterHelper::Single(_) => self.next(),
            NodeIterHelper::Vec(ref mut iter) => iter.next_back(),
        }
    }
}

impl<'a, 'text, S: SourceTextAllocator<'text, 'a>> FlattenOp
    for DirectDeclSmartConstructors<'a, 'text, S>
{
    type S = Node<'a>;

    fn flatten(&self, kind: SyntaxKind, lst: std::vec::Vec<Self::S>) -> Self::S {
        let size = lst
            .iter()
            .map(|s| match s {
                Node::List(children) => children.len(),
                x => {
                    if Self::is_zero(x) {
                        0
                    } else {
                        1
                    }
                }
            })
            .sum();
        let mut r = Vec::with_capacity_in(size, self.arena);
        for s in lst.into_iter() {
            match s {
                Node::List(children) => r.extend(children.iter().copied()),
                x => {
                    if !Self::is_zero(&x) {
                        r.push(x)
                    }
                }
            }
        }
        match r.into_bump_slice() {
            [] => Node::Ignored(kind),
            [node] => *node,
            slice => Node::List(self.alloc(slice)),
        }
    }

    fn zero(kind: SyntaxKind) -> Self::S {
        Node::Ignored(kind)
    }

    fn is_zero(s: &Self::S) -> bool {
        match s {
            Node::Token(token) => match token.kind() {
                TokenKind::Yield | TokenKind::Required | TokenKind::Lateinit => false,
                _ => true,
            },
            Node::List(inner) => inner.iter().all(Self::is_zero),
            _ => true,
        }
    }
}

impl<'a, 'text, S: SourceTextAllocator<'text, 'a>>
    FlattenSmartConstructors<'a, DirectDeclSmartConstructors<'a, 'text, S>>
    for DirectDeclSmartConstructors<'a, 'text, S>
{
    fn make_token(&mut self, token: CompactToken) -> Self::R {
        let token_text = |this: &Self| this.str_from_utf8(this.token_bytes(&token));
        let token_pos = |this: &Self| {
            let start = this
                .source_text
                .offset_to_file_pos_triple(token.start_offset());
            let end = this
                .source_text
                .offset_to_file_pos_triple(token.end_offset());
            Pos::from_lnum_bol_cnum(this.arena, this.filename, start, end)
        };
        let kind = token.kind();

        let result = match kind {
            TokenKind::Name | TokenKind::XHPClassName => {
                let text = token_text(self);
                let pos = token_pos(self);

                let name = if kind == TokenKind::XHPClassName {
                    Node::XhpName(self.alloc((text, pos)))
                } else {
                    Node::Name(self.alloc((text, pos)))
                };

                if self.previous_token_kind == TokenKind::Class
                    || self.previous_token_kind == TokenKind::Trait
                    || self.previous_token_kind == TokenKind::Interface
                {
                    if let Some(current_class_name) = self.elaborate_defined_id(name) {
                        self.classish_name_builder
                            .lexed_name_after_classish_keyword(
                                self.arena,
                                current_class_name.1,
                                pos,
                                self.previous_token_kind,
                            );
                    }
                }
                name
            }
            TokenKind::Class => Node::Name(self.alloc((token_text(self), token_pos(self)))),
            TokenKind::Variable => Node::Variable(self.alloc((token_text(self), token_pos(self)))),
            // There are a few types whose string representations we have to
            // grab anyway, so just go ahead and treat them as generic names.
            TokenKind::Vec
            | TokenKind::Dict
            | TokenKind::Keyset
            | TokenKind::Tuple
            | TokenKind::Classname
            | TokenKind::SelfToken => Node::Name(self.alloc((token_text(self), token_pos(self)))),
            TokenKind::XHPElementName => {
                Node::XhpName(self.alloc((token_text(self), token_pos(self))))
            }
            TokenKind::SingleQuotedStringLiteral => match escaper::unescape_single_in(
                self.str_from_utf8(escaper::unquote_slice(self.token_bytes(&token))),
                self.arena,
            ) {
                Ok(text) => Node::StringLiteral(self.alloc((text.into(), token_pos(self)))),
                Err(_) => Node::Ignored(SK::Token(kind)),
            },
            TokenKind::DoubleQuotedStringLiteral => match escaper::unescape_double_in(
                self.str_from_utf8(escaper::unquote_slice(self.token_bytes(&token))),
                self.arena,
            ) {
                Ok(text) => Node::StringLiteral(self.alloc((text.into(), token_pos(self)))),
                Err(_) => Node::Ignored(SK::Token(kind)),
            },
            TokenKind::HeredocStringLiteral => match escaper::unescape_heredoc_in(
                self.str_from_utf8(escaper::unquote_slice(self.token_bytes(&token))),
                self.arena,
            ) {
                Ok(text) => Node::StringLiteral(self.alloc((text.into(), token_pos(self)))),
                Err(_) => Node::Ignored(SK::Token(kind)),
            },
            TokenKind::NowdocStringLiteral => match escaper::unescape_nowdoc_in(
                self.str_from_utf8(escaper::unquote_slice(self.token_bytes(&token))),
                self.arena,
            ) {
                Ok(text) => Node::StringLiteral(self.alloc((text.into(), token_pos(self)))),
                Err(_) => Node::Ignored(SK::Token(kind)),
            },
            TokenKind::DecimalLiteral
            | TokenKind::OctalLiteral
            | TokenKind::HexadecimalLiteral
            | TokenKind::BinaryLiteral => {
                Node::IntLiteral(self.alloc((token_text(self), token_pos(self))))
            }
            TokenKind::FloatingLiteral => {
                Node::FloatingLiteral(self.alloc((token_text(self), token_pos(self))))
            }
            TokenKind::BooleanLiteral => {
                Node::BooleanLiteral(self.alloc((token_text(self), token_pos(self))))
            }
            TokenKind::String => self.prim_ty(aast::Tprim::Tstring, token_pos(self)),
            TokenKind::Int => self.prim_ty(aast::Tprim::Tint, token_pos(self)),
            TokenKind::Float => self.prim_ty(aast::Tprim::Tfloat, token_pos(self)),
            // "double" and "boolean" are parse errors--they should be written
            // "float" and "bool". The decl-parser treats the incorrect names as
            // type names rather than primitives.
            TokenKind::Double | TokenKind::Boolean => self.hint_ty(
                token_pos(self),
                Ty_::Tapply(self.alloc(((token_pos(self), token_text(self)), &[][..]))),
            ),
            TokenKind::Num => self.prim_ty(aast::Tprim::Tnum, token_pos(self)),
            TokenKind::Bool => self.prim_ty(aast::Tprim::Tbool, token_pos(self)),
            TokenKind::Mixed => {
                Node::Ty(self.alloc(Ty(self.alloc(Reason::hint(token_pos(self))), Ty_::Tmixed)))
            }
            TokenKind::Void => self.prim_ty(aast::Tprim::Tvoid, token_pos(self)),
            TokenKind::Arraykey => self.prim_ty(aast::Tprim::Tarraykey, token_pos(self)),
            TokenKind::Noreturn => self.prim_ty(aast::Tprim::Tnoreturn, token_pos(self)),
            TokenKind::Resource => self.prim_ty(aast::Tprim::Tresource, token_pos(self)),
            TokenKind::NullLiteral
            | TokenKind::Darray
            | TokenKind::Varray
            | TokenKind::Backslash
            | TokenKind::Construct
            | TokenKind::LeftParen
            | TokenKind::RightParen
            | TokenKind::LeftBracket
            | TokenKind::RightBracket
            | TokenKind::Shape
            | TokenKind::Question
            | TokenKind::This
            | TokenKind::Tilde
            | TokenKind::Exclamation
            | TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::PlusPlus
            | TokenKind::MinusMinus
            | TokenKind::At
            | TokenKind::Star
            | TokenKind::Slash
            | TokenKind::EqualEqual
            | TokenKind::EqualEqualEqual
            | TokenKind::StarStar
            | TokenKind::AmpersandAmpersand
            | TokenKind::BarBar
            | TokenKind::LessThan
            | TokenKind::LessThanEqual
            | TokenKind::GreaterThan
            | TokenKind::GreaterThanEqual
            | TokenKind::Dot
            | TokenKind::Ampersand
            | TokenKind::Bar
            | TokenKind::LessThanLessThan
            | TokenKind::GreaterThanGreaterThan
            | TokenKind::Percent
            | TokenKind::QuestionQuestion
            | TokenKind::Equal
            | TokenKind::Abstract
            | TokenKind::As
            | TokenKind::Super
            | TokenKind::Async
            | TokenKind::DotDotDot
            | TokenKind::Extends
            | TokenKind::Final
            | TokenKind::Implements
            | TokenKind::Inout
            | TokenKind::Interface
            | TokenKind::Newctx
            | TokenKind::Newtype
            | TokenKind::Type
            | TokenKind::Yield
            | TokenKind::Semicolon
            | TokenKind::Private
            | TokenKind::Protected
            | TokenKind::Public
            | TokenKind::Reify
            | TokenKind::Static
            | TokenKind::Trait
            | TokenKind::Lateinit
            | TokenKind::RecordDec
            | TokenKind::RightBrace
            | TokenKind::Enum
            | TokenKind::Const
            | TokenKind::Function
            | TokenKind::Namespace
            | TokenKind::XHP
            | TokenKind::Required
            | TokenKind::Ctx
            | TokenKind::Readonly => Node::Token(FixedWidthToken::new(kind, token.start_offset())),
            TokenKind::EndOfFile
            | TokenKind::Attribute
            | TokenKind::Await
            | TokenKind::Binary
            | TokenKind::Break
            | TokenKind::Case
            | TokenKind::Catch
            | TokenKind::Category
            | TokenKind::Children
            | TokenKind::Clone
            | TokenKind::Continue
            | TokenKind::Default
            | TokenKind::Define
            | TokenKind::Do
            | TokenKind::Echo
            | TokenKind::Else
            | TokenKind::Elseif
            | TokenKind::Empty
            | TokenKind::Endfor
            | TokenKind::Endforeach
            | TokenKind::Endif
            | TokenKind::Endswitch
            | TokenKind::Endwhile
            | TokenKind::Eval
            | TokenKind::Fallthrough
            | TokenKind::File
            | TokenKind::Finally
            | TokenKind::For
            | TokenKind::Foreach
            | TokenKind::From
            | TokenKind::Global
            | TokenKind::Concurrent
            | TokenKind::If
            | TokenKind::Include
            | TokenKind::Include_once
            | TokenKind::Instanceof
            | TokenKind::Insteadof
            | TokenKind::Integer
            | TokenKind::Is
            | TokenKind::Isset
            | TokenKind::List
            | TokenKind::New
            | TokenKind::Object
            | TokenKind::Parent
            | TokenKind::Print
            | TokenKind::Real
            | TokenKind::Record
            | TokenKind::Require
            | TokenKind::Require_once
            | TokenKind::Return
            | TokenKind::Switch
            | TokenKind::Throw
            | TokenKind::Try
            | TokenKind::Unset
            | TokenKind::Upcast
            | TokenKind::Use
            | TokenKind::Using
            | TokenKind::Var
            | TokenKind::Where
            | TokenKind::While
            | TokenKind::LeftBrace
            | TokenKind::MinusGreaterThan
            | TokenKind::Dollar
            | TokenKind::LessThanEqualGreaterThan
            | TokenKind::ExclamationEqual
            | TokenKind::ExclamationEqualEqual
            | TokenKind::Carat
            | TokenKind::QuestionAs
            | TokenKind::QuestionColon
            | TokenKind::QuestionQuestionEqual
            | TokenKind::Colon
            | TokenKind::StarStarEqual
            | TokenKind::StarEqual
            | TokenKind::SlashEqual
            | TokenKind::PercentEqual
            | TokenKind::PlusEqual
            | TokenKind::MinusEqual
            | TokenKind::DotEqual
            | TokenKind::LessThanLessThanEqual
            | TokenKind::GreaterThanGreaterThanEqual
            | TokenKind::AmpersandEqual
            | TokenKind::CaratEqual
            | TokenKind::BarEqual
            | TokenKind::Comma
            | TokenKind::ColonColon
            | TokenKind::EqualGreaterThan
            | TokenKind::EqualEqualGreaterThan
            | TokenKind::QuestionMinusGreaterThan
            | TokenKind::DollarDollar
            | TokenKind::BarGreaterThan
            | TokenKind::SlashGreaterThan
            | TokenKind::LessThanSlash
            | TokenKind::LessThanQuestion
            | TokenKind::Backtick
            | TokenKind::ErrorToken
            | TokenKind::DoubleQuotedStringLiteralHead
            | TokenKind::StringLiteralBody
            | TokenKind::DoubleQuotedStringLiteralTail
            | TokenKind::HeredocStringLiteralHead
            | TokenKind::HeredocStringLiteralTail
            | TokenKind::XHPCategoryName
            | TokenKind::XHPStringLiteral
            | TokenKind::XHPBody
            | TokenKind::XHPComment
            | TokenKind::Hash
            | TokenKind::Hashbang => Node::Ignored(SK::Token(kind)),
        };
        self.previous_token_kind = kind;
        result
    }

    fn make_missing(&mut self, _: usize) -> Self::R {
        Node::Ignored(SK::Missing)
    }

    fn make_list(&mut self, items: std::vec::Vec<Self::R>, _: usize) -> Self::R {
        if let Some(&yield_) = items
            .iter()
            .flat_map(|node| node.iter())
            .find(|node| node.is_token(TokenKind::Yield))
        {
            yield_
        } else {
            let size = items.iter().filter(|node| node.is_present()).count();
            let items_iter = items.into_iter();
            let mut items = Vec::with_capacity_in(size, self.arena);
            for node in items_iter {
                if node.is_present() {
                    items.push(node);
                }
            }
            let items = items.into_bump_slice();
            if items.is_empty() {
                Node::Ignored(SK::SyntaxList)
            } else {
                Node::List(self.alloc(items))
            }
        }
    }

    fn make_qualified_name(&mut self, parts: Self::R) -> Self::R {
        let pos = self.get_pos(parts);
        match parts {
            Node::List(nodes) => Node::QualifiedName(self.alloc((nodes, pos))),
            node if node.is_ignored() => Node::Ignored(SK::QualifiedName),
            node => Node::QualifiedName(
                self.alloc((bumpalo::vec![in self.arena; node].into_bump_slice(), pos)),
            ),
        }
    }

    fn make_simple_type_specifier(&mut self, specifier: Self::R) -> Self::R {
        // Return this explicitly because flatten filters out zero nodes, and
        // we treat most non-error nodes as zeroes.
        specifier
    }

    fn make_literal_expression(&mut self, expression: Self::R) -> Self::R {
        expression
    }

    fn make_simple_initializer(&mut self, equals: Self::R, expr: Self::R) -> Self::R {
        // If the expr is Ignored, bubble up the assignment operator so that we
        // can tell that *some* initializer was here. Useful for class
        // properties, where we need to enforce that properties without default
        // values are initialized in the constructor.
        if expr.is_ignored() { equals } else { expr }
    }

    fn make_anonymous_function(
        &mut self,
        _attribute_spec: Self::R,
        _async_keyword: Self::R,
        _function_keyword: Self::R,
        _left_paren: Self::R,
        _parameters: Self::R,
        _right_paren: Self::R,
        _ctx_list: Self::R,
        _colon: Self::R,
        _readonly_return: Self::R,
        _type_: Self::R,
        _use_: Self::R,
        _body: Self::R,
    ) -> Self::R {
        // do not allow Yield to bubble up
        Node::Ignored(SK::AnonymousFunction)
    }

    fn make_lambda_expression(
        &mut self,
        _attribute_spec: Self::R,
        _async_: Self::R,
        _signature: Self::R,
        _arrow: Self::R,
        _body: Self::R,
    ) -> Self::R {
        // do not allow Yield to bubble up
        Node::Ignored(SK::LambdaExpression)
    }

    fn make_awaitable_creation_expression(
        &mut self,
        _attribute_spec: Self::R,
        _async_: Self::R,
        _compound_statement: Self::R,
    ) -> Self::R {
        // do not allow Yield to bubble up
        Node::Ignored(SK::AwaitableCreationExpression)
    }

    fn make_element_initializer(
        &mut self,
        key: Self::R,
        _arrow: Self::R,
        value: Self::R,
    ) -> Self::R {
        Node::ListItem(self.alloc((key, value)))
    }

    fn make_prefix_unary_expression(&mut self, op: Self::R, value: Self::R) -> Self::R {
        let pos = self.merge_positions(op, value);
        let op = match op.token_kind() {
            Some(TokenKind::Tilde) => Uop::Utild,
            Some(TokenKind::Exclamation) => Uop::Unot,
            Some(TokenKind::Plus) => Uop::Uplus,
            Some(TokenKind::Minus) => Uop::Uminus,
            Some(TokenKind::PlusPlus) => Uop::Uincr,
            Some(TokenKind::MinusMinus) => Uop::Udecr,
            Some(TokenKind::At) => Uop::Usilence,
            _ => return Node::Ignored(SK::PrefixUnaryExpression),
        };
        let value = match self.node_to_expr(value) {
            Some(value) => value,
            None => return Node::Ignored(SK::PrefixUnaryExpression),
        };
        Node::Expr(self.alloc(aast::Expr(
            (),
            pos,
            aast::Expr_::Unop(self.alloc((op, value))),
        )))
    }

    fn make_postfix_unary_expression(&mut self, value: Self::R, op: Self::R) -> Self::R {
        let pos = self.merge_positions(value, op);
        let op = match op.token_kind() {
            Some(TokenKind::PlusPlus) => Uop::Upincr,
            Some(TokenKind::MinusMinus) => Uop::Updecr,
            _ => return Node::Ignored(SK::PostfixUnaryExpression),
        };
        let value = match self.node_to_expr(value) {
            Some(value) => value,
            None => return Node::Ignored(SK::PostfixUnaryExpression),
        };
        Node::Expr(self.alloc(aast::Expr(
            (),
            pos,
            aast::Expr_::Unop(self.alloc((op, value))),
        )))
    }

    fn make_binary_expression(&mut self, lhs: Self::R, op_node: Self::R, rhs: Self::R) -> Self::R {
        let op = match op_node.token_kind() {
            Some(TokenKind::Plus) => Bop::Plus,
            Some(TokenKind::Minus) => Bop::Minus,
            Some(TokenKind::Star) => Bop::Star,
            Some(TokenKind::Slash) => Bop::Slash,
            Some(TokenKind::Equal) => Bop::Eq(None),
            Some(TokenKind::EqualEqual) => Bop::Eqeq,
            Some(TokenKind::EqualEqualEqual) => Bop::Eqeqeq,
            Some(TokenKind::StarStar) => Bop::Starstar,
            Some(TokenKind::AmpersandAmpersand) => Bop::Ampamp,
            Some(TokenKind::BarBar) => Bop::Barbar,
            Some(TokenKind::LessThan) => Bop::Lt,
            Some(TokenKind::LessThanEqual) => Bop::Lte,
            Some(TokenKind::LessThanLessThan) => Bop::Ltlt,
            Some(TokenKind::GreaterThan) => Bop::Gt,
            Some(TokenKind::GreaterThanEqual) => Bop::Gte,
            Some(TokenKind::GreaterThanGreaterThan) => Bop::Gtgt,
            Some(TokenKind::Dot) => Bop::Dot,
            Some(TokenKind::Ampersand) => Bop::Amp,
            Some(TokenKind::Bar) => Bop::Bar,
            Some(TokenKind::Percent) => Bop::Percent,
            Some(TokenKind::QuestionQuestion) => Bop::QuestionQuestion,
            _ => return Node::Ignored(SK::BinaryExpression),
        };

        match (&op, rhs.is_token(TokenKind::Yield)) {
            (Bop::Eq(_), true) => return rhs,
            _ => {}
        }

        let pos = self.merge(self.merge_positions(lhs, op_node), self.get_pos(rhs));

        let lhs = match self.node_to_expr(lhs) {
            Some(lhs) => lhs,
            None => return Node::Ignored(SK::BinaryExpression),
        };
        let rhs = match self.node_to_expr(rhs) {
            Some(rhs) => rhs,
            None => return Node::Ignored(SK::BinaryExpression),
        };

        Node::Expr(self.alloc(aast::Expr(
            (),
            pos,
            aast::Expr_::Binop(self.alloc((op, lhs, rhs))),
        )))
    }

    fn make_parenthesized_expression(
        &mut self,
        _lparen: Self::R,
        expr: Self::R,
        _rparen: Self::R,
    ) -> Self::R {
        expr
    }

    fn make_list_item(&mut self, item: Self::R, sep: Self::R) -> Self::R {
        match (item.is_ignored(), sep.is_ignored()) {
            (true, true) => Node::Ignored(SK::ListItem),
            (false, true) => item,
            (true, false) => sep,
            (false, false) => Node::ListItem(self.alloc((item, sep))),
        }
    }

    fn make_type_arguments(
        &mut self,
        less_than: Self::R,
        arguments: Self::R,
        greater_than: Self::R,
    ) -> Self::R {
        Node::BracketedList(self.alloc((
            self.get_pos(less_than),
            arguments.as_slice(self.arena),
            self.get_pos(greater_than),
        )))
    }

    fn make_generic_type_specifier(
        &mut self,
        class_type: Self::R,
        type_arguments: Self::R,
    ) -> Self::R {
        let class_id = match self.expect_name(class_type) {
            Some(id) => id,
            None => return Node::Ignored(SK::GenericTypeSpecifier),
        };
        match class_id.1.trim_start_matches("\\") {
            "varray_or_darray" | "vec_or_dict" => {
                let id_pos = class_id.0;
                let pos = self.merge(id_pos, self.get_pos(type_arguments));
                let type_arguments = type_arguments.as_slice(self.arena);
                let ty_ = match type_arguments {
                    [tk, tv] => Ty_::TvecOrDict(
                        self.alloc((
                            self.node_to_ty(*tk)
                                .unwrap_or_else(|| self.tany_with_pos(id_pos)),
                            self.node_to_ty(*tv)
                                .unwrap_or_else(|| self.tany_with_pos(id_pos)),
                        )),
                    ),
                    [tv] => Ty_::TvecOrDict(
                        self.alloc((
                            self.vec_or_dict_key(pos),
                            self.node_to_ty(*tv)
                                .unwrap_or_else(|| self.tany_with_pos(id_pos)),
                        )),
                    ),
                    _ => TANY_,
                };
                self.hint_ty(pos, ty_)
            }
            _ => {
                let Id(pos, class_type) = class_id;
                match class_type.rsplit('\\').next() {
                    Some(name) if self.is_type_param_in_scope(name) => {
                        let pos = self.merge(pos, self.get_pos(type_arguments));
                        let type_arguments = self.slice(
                            type_arguments
                                .iter()
                                .filter_map(|&node| self.node_to_ty(node)),
                        );
                        let ty_ = Ty_::Tgeneric(self.alloc((name, type_arguments)));
                        self.hint_ty(pos, ty_)
                    }
                    _ => {
                        let class_type = self.elaborate_raw_id(class_type);
                        self.make_apply(
                            (pos, class_type),
                            type_arguments,
                            self.get_pos(type_arguments),
                        )
                    }
                }
            }
        }
    }

    fn make_record_declaration(
        &mut self,
        attribute_spec: Self::R,
        modifier: Self::R,
        record_keyword: Self::R,
        name: Self::R,
        _extends_keyword: Self::R,
        extends_opt: Self::R,
        _left_brace: Self::R,
        fields: Self::R,
        right_brace: Self::R,
    ) -> Self::R {
        let name = match self.elaborate_defined_id(name) {
            Some(name) => name,
            None => return Node::Ignored(SK::RecordDeclaration),
        };
        self.add_record(
            name.1,
            self.alloc(typing_defs::RecordDefType {
                module: &None, // TODO: grab module from attributes
                name: name.into(),
                extends: self
                    .expect_name(extends_opt)
                    .map(|id| self.elaborate_id(id).into()),
                fields: self.slice(fields.iter().filter_map(|node| match node {
                    Node::RecordField(&(id, req)) => Some((id.into(), req)),
                    _ => None,
                })),
                abstract_: modifier.is_token(TokenKind::Abstract),
                pos: self.pos_from_slice(&[attribute_spec, modifier, record_keyword, right_brace]),
            }),
        );
        Node::Ignored(SK::RecordDeclaration)
    }

    fn make_record_field(
        &mut self,
        _type_: Self::R,
        name: Self::R,
        initializer: Self::R,
        _semicolon: Self::R,
    ) -> Self::R {
        let name = match self.expect_name(name) {
            Some(name) => name,
            None => return Node::Ignored(SK::RecordField),
        };
        let field_req = if initializer.is_ignored() {
            RecordFieldReq::ValueRequired
        } else {
            RecordFieldReq::HasDefaultValue
        };
        Node::RecordField(self.alloc((name, field_req)))
    }

    fn make_alias_declaration(
        &mut self,
        attributes: Self::R,
        keyword: Self::R,
        name: Self::R,
        generic_params: Self::R,
        constraint: Self::R,
        _equal: Self::R,
        aliased_type: Self::R,
        _semicolon: Self::R,
    ) -> Self::R {
        if name.is_ignored() {
            return Node::Ignored(SK::AliasDeclaration);
        }
        let Id(pos, name) = match self.elaborate_defined_id(name) {
            Some(id) => id,
            None => return Node::Ignored(SK::AliasDeclaration),
        };
        let ty = match self.node_to_ty(aliased_type) {
            Some(ty) => ty,
            None => return Node::Ignored(SK::AliasDeclaration),
        };
        let constraint = match constraint {
            Node::TypeConstraint(&(_kind, hint)) => self.node_to_ty(hint),
            _ => None,
        };
        // Pop the type params stack only after creating all inner types.
        let tparams = self.pop_type_params(generic_params);
        let parsed_attributes = self.to_attributes(attributes);
        let typedef = self.alloc(TypedefType {
            module: self.alloc(parsed_attributes.module),
            pos,
            vis: if parsed_attributes.internal {
                aast::TypedefVisibility::Tinternal
            } else {
                match keyword.token_kind() {
                    Some(TokenKind::Type) => aast::TypedefVisibility::Transparent,
                    Some(TokenKind::Newtype) => aast::TypedefVisibility::Opaque,
                    _ => aast::TypedefVisibility::Transparent,
                }
            },
            tparams,
            constraint,
            type_: ty,
            is_ctx: false,
        });

        self.add_typedef(name, typedef);

        Node::Ignored(SK::AliasDeclaration)
    }

    fn make_context_alias_declaration(
        &mut self,
        attributes: Self::R,
        _keyword: Self::R,
        name: Self::R,
        generic_params: Self::R,
        constraint: Self::R,
        _equal: Self::R,
        ctx_list: Self::R,
        _semicolon: Self::R,
    ) -> Self::R {
        if name.is_ignored() {
            return Node::Ignored(SK::ContextAliasDeclaration);
        }
        let Id(pos, name) = match self.elaborate_defined_id(name) {
            Some(id) => id,
            None => return Node::Ignored(SK::ContextAliasDeclaration),
        };
        let ty = match self.node_to_ty(ctx_list) {
            Some(ty) => ty,
            None => self.alloc(Ty(
                self.alloc(Reason::hint(pos)),
                Ty_::Tapply(self.alloc(((pos, "\\HH\\Contexts\\defaults"), &[]))),
            )),
        };

        // lowerer ensures there is only one as constraint
        let mut as_constraint = None;
        for c in constraint.iter() {
            if let Node::ContextConstraint(&(kind, hint)) = c {
                let ty = self.node_to_ty(hint);
                match kind {
                    ConstraintKind::ConstraintAs => as_constraint = ty,
                    _ => {}
                }
            }
        }
        // Pop the type params stack only after creating all inner types.
        let tparams = self.pop_type_params(generic_params);
        let parsed_attributes = self.to_attributes(attributes);
        let typedef = self.alloc(TypedefType {
            module: self.alloc(parsed_attributes.module),
            pos,
            vis: if parsed_attributes.internal {
                aast::TypedefVisibility::Tinternal
            } else {
                aast::TypedefVisibility::Opaque
            },
            tparams,
            constraint: as_constraint,
            type_: ty,
            is_ctx: true,
        });

        self.add_typedef(name, typedef);

        Node::Ignored(SK::ContextAliasDeclaration)
    }

    fn make_type_constraint(&mut self, kind: Self::R, value: Self::R) -> Self::R {
        let kind = match kind.token_kind() {
            Some(TokenKind::As) => ConstraintKind::ConstraintAs,
            Some(TokenKind::Super) => ConstraintKind::ConstraintSuper,
            _ => return Node::Ignored(SK::TypeConstraint),
        };
        Node::TypeConstraint(self.alloc((kind, value)))
    }

    fn make_context_constraint(&mut self, kind: Self::R, value: Self::R) -> Self::R {
        let kind = match kind.token_kind() {
            Some(TokenKind::As) => ConstraintKind::ConstraintAs,
            Some(TokenKind::Super) => ConstraintKind::ConstraintSuper,
            _ => return Node::Ignored(SK::ContextConstraint),
        };
        Node::ContextConstraint(self.alloc((kind, value)))
    }

    fn make_type_parameter(
        &mut self,
        user_attributes: Self::R,
        reify: Self::R,
        variance: Self::R,
        name: Self::R,
        tparam_params: Self::R,
        constraints: Self::R,
    ) -> Self::R {
        let user_attributes = match user_attributes {
            Node::BracketedList((_, attributes, _)) => {
                self.slice(attributes.into_iter().filter_map(|x| match x {
                    Node::Attribute(a) => Some(*a),
                    _ => None,
                }))
            }
            _ => &[][..],
        };

        let constraints = self.slice(constraints.iter().filter_map(|node| match node {
            Node::TypeConstraint(&constraint) => Some(constraint),
            _ => None,
        }));

        // TODO(T70068435) Once we add support for constraints on higher-kinded types
        // (in particular, constraints on nested type parameters), we need to ensure
        // that we correctly handle the scoping of nested type parameters.
        // This includes making sure that the call to convert_type_appl_to_generic
        // in make_type_parameters handles nested constraints.
        // For now, we just make sure that the nested type parameters that make_type_parameters
        // added to the global list of in-scope type parameters are removed immediately:
        self.pop_type_params(tparam_params);

        let tparam_params = match tparam_params {
            Node::TypeParameters(&params) => params,
            _ => &[],
        };

        Node::TypeParameter(self.alloc(TypeParameterDecl {
            name,
            variance: match variance.token_kind() {
                Some(TokenKind::Minus) => Variance::Contravariant,
                Some(TokenKind::Plus) => Variance::Covariant,
                _ => Variance::Invariant,
            },
            reified: if reify.is_token(TokenKind::Reify) {
                if user_attributes.iter().any(|node| node.name.1 == "__Soft") {
                    aast::ReifyKind::SoftReified
                } else {
                    aast::ReifyKind::Reified
                }
            } else {
                aast::ReifyKind::Erased
            },
            constraints,
            tparam_params,
            user_attributes,
        }))
    }

    fn make_type_parameters(&mut self, _lt: Self::R, tparams: Self::R, _gt: Self::R) -> Self::R {
        let size = tparams.len();
        let mut tparams_with_name = Vec::with_capacity_in(size, self.arena);
        let mut tparam_names = MultiSetMut::with_capacity_in(size, self.arena);
        for node in tparams.iter() {
            match node {
                &Node::TypeParameter(decl) => {
                    let name = match decl.name.as_id() {
                        Some(name) => name,
                        None => return Node::Ignored(SK::TypeParameters),
                    };
                    tparam_names.insert(name.1);
                    tparams_with_name.push((decl, name));
                }
                _ => {}
            }
        }
        Rc::make_mut(&mut self.type_parameters).push(tparam_names.into());
        let mut tparams = Vec::with_capacity_in(tparams_with_name.len(), self.arena);
        for (decl, name) in tparams_with_name.into_iter() {
            let &TypeParameterDecl {
                name: _,
                variance,
                reified,
                constraints,
                tparam_params,
                user_attributes,
            } = decl;
            let constraints = self.slice(constraints.iter().filter_map(|constraint| {
                let &(kind, ty) = constraint;
                let ty = self.node_to_ty(ty)?;
                let ty = self.convert_tapply_to_tgeneric(ty);
                Some((kind, ty))
            }));

            let user_attributes = self.slice(
                user_attributes
                    .iter()
                    .rev()
                    .map(|x| self.user_attribute_to_decl(x)),
            );
            tparams.push(self.alloc(Tparam {
                variance,
                name: name.into(),
                constraints,
                reified,
                user_attributes,
                tparams: tparam_params,
            }));
        }
        Node::TypeParameters(self.alloc(tparams.into_bump_slice()))
    }

    fn make_parameter_declaration(
        &mut self,
        attributes: Self::R,
        visibility: Self::R,
        inout: Self::R,
        readonly: Self::R,
        hint: Self::R,
        name: Self::R,
        initializer: Self::R,
    ) -> Self::R {
        let (variadic, pos, name) = match name {
            Node::ListItem(&(ellipsis, id)) => {
                let Id(pos, name) = match id.as_variable() {
                    Some(id) => id,
                    None => return Node::Ignored(SK::ParameterDeclaration),
                };
                let variadic = ellipsis.is_token(TokenKind::DotDotDot);
                (variadic, pos, Some(name))
            }
            name => {
                let Id(pos, name) = match name.as_variable() {
                    Some(id) => id,
                    None => return Node::Ignored(SK::ParameterDeclaration),
                };
                (false, pos, Some(name))
            }
        };
        let kind = if inout.is_token(TokenKind::Inout) {
            ParamMode::FPinout
        } else {
            ParamMode::FPnormal
        };
        let is_readonly = readonly.is_token(TokenKind::Readonly);
        let hint = if self.opts.interpret_soft_types_as_like_types {
            let attributes = self.to_attributes(attributes);
            if attributes.soft {
                match hint {
                    Node::Ty(ty) => self.hint_ty(self.get_pos(hint), Ty_::Tlike(ty)),
                    _ => hint,
                }
            } else {
                hint
            }
        } else {
            hint
        };
        Node::FunParam(self.alloc(FunParamDecl {
            attributes,
            visibility,
            kind,
            readonly: is_readonly,
            hint,
            pos,
            name,
            variadic,
            initializer,
        }))
    }

    fn make_variadic_parameter(&mut self, _: Self::R, hint: Self::R, ellipsis: Self::R) -> Self::R {
        Node::FunParam(
            self.alloc(FunParamDecl {
                attributes: Node::Ignored(SK::Missing),
                visibility: Node::Ignored(SK::Missing),
                kind: ParamMode::FPnormal,
                readonly: false,
                hint,
                pos: self
                    .get_pos_opt(hint)
                    .unwrap_or_else(|| self.get_pos(ellipsis)),
                name: None,
                variadic: true,
                initializer: Node::Ignored(SK::Missing),
            }),
        )
    }

    fn make_function_declaration(
        &mut self,
        attributes: Self::R,
        header: Self::R,
        body: Self::R,
    ) -> Self::R {
        let parsed_attributes = self.to_attributes(attributes);
        match header {
            Node::FunctionHeader(header) => {
                let is_method = false;
                let ((pos, name), type_, _) =
                    match self.function_to_ty(is_method, attributes, header, body) {
                        Some(x) => x,
                        None => return Node::Ignored(SK::FunctionDeclaration),
                    };
                let deprecated = parsed_attributes.deprecated.map(|msg| {
                    let mut s = String::new_in(self.arena);
                    s.push_str("The function ");
                    s.push_str(name.trim_start_matches("\\"));
                    s.push_str(" is deprecated: ");
                    s.push_str(msg);
                    s.into_bump_str()
                });
                let fun_elt = self.alloc(FunElt {
                    module: self.alloc(parsed_attributes.module),
                    internal: parsed_attributes.internal,
                    deprecated,
                    type_,
                    pos,
                    php_std_lib: parsed_attributes.php_std_lib,
                    support_dynamic_type: self.opts.everything_sdt
                        || parsed_attributes.support_dynamic_type,
                });
                self.add_fun(name, fun_elt);
                Node::Ignored(SK::FunctionDeclaration)
            }
            _ => Node::Ignored(SK::FunctionDeclaration),
        }
    }

    fn make_contexts(
        &mut self,
        left_bracket: Self::R,
        tys: Self::R,
        right_bracket: Self::R,
    ) -> Self::R {
        let tys = self.slice(tys.iter().filter_map(|ty| match ty {
            Node::ListItem(&(ty, _)) | &ty => {
                // A wildcard is used for the context of a closure type on a
                // parameter of a function with a function context (e.g.,
                // `function f((function ()[_]: void) $f)[ctx $f]: void {}`).
                if let Some(Id(pos, "_")) = self.expect_name(ty) {
                    return Some(self.alloc(Ty(
                        self.alloc(Reason::hint(pos)),
                        Ty_::Tapply(self.alloc(((pos, "_"), &[]))),
                    )));
                }
                let ty = self.node_to_ty(ty)?;
                match ty.1 {
                    // Only three forms of type can appear here in a valid program:
                    //   - function contexts (`ctx $f`)
                    //   - value-dependent paths (`$v::C`)
                    //   - built-in contexts (`rx`, `cipp_of<EntFoo>`)
                    // The first and last will be represented with `Tapply`,
                    // but function contexts will use a variable name
                    // (containing a `$`). Built-in contexts are always in the
                    // \HH\Contexts namespace, so we rewrite those names here.
                    Ty_::Tapply(&((pos, name), targs)) if !name.starts_with('$') => {
                        // The name will have been elaborated in the current
                        // namespace, but we actually want it to be in the
                        // \HH\Contexts namespace. Grab the last component of
                        // the name, and rewrite it in the correct namespace.
                        // Note that this makes it impossible to express names
                        // in any sub-namespace of \HH\Contexts (e.g.,
                        // "Unsafe\\cipp" will be rewritten as
                        // "\\HH\\Contexts\\cipp" rather than
                        // "\\HH\\Contexts\\Unsafe\\cipp").
                        let name = match name.trim_end_matches('\\').split('\\').next_back() {
                            Some(ctxname) => {
                                if let Some(first_char) = ctxname.chars().nth(0) {
                                    if first_char.is_lowercase() {
                                        self.concat("\\HH\\Contexts\\", ctxname)
                                    } else {
                                        name
                                    }
                                } else {
                                    name
                                }
                            }
                            None => name,
                        };
                        Some(self.alloc(Ty(ty.0, Ty_::Tapply(self.alloc(((pos, name), targs))))))
                    }
                    _ => Some(ty),
                }
            }
        }));
        /* Like in as_fun_implicit_params, we keep the intersection as is: we do not simplify
         * empty or singleton intersections.
         */
        let pos = self.merge_positions(left_bracket, right_bracket);
        self.hint_ty(pos, Ty_::Tintersection(tys))
    }

    fn make_function_ctx_type_specifier(
        &mut self,
        ctx_keyword: Self::R,
        variable: Self::R,
    ) -> Self::R {
        match variable.as_variable() {
            Some(Id(pos, name)) => {
                Node::Variable(self.alloc((name, self.merge(pos, self.get_pos(ctx_keyword)))))
            }
            None => Node::Ignored(SK::FunctionCtxTypeSpecifier),
        }
    }

    fn make_function_declaration_header(
        &mut self,
        modifiers: Self::R,
        _keyword: Self::R,
        name: Self::R,
        type_params: Self::R,
        left_paren: Self::R,
        param_list: Self::R,
        _right_paren: Self::R,
        capability: Self::R,
        _colon: Self::R,
        readonly_return: Self::R,
        ret_hint: Self::R,
        where_constraints: Self::R,
    ) -> Self::R {
        // Use the position of the left paren if the name is missing.
        let name = if name.is_ignored() { left_paren } else { name };
        Node::FunctionHeader(self.alloc(FunctionHeader {
            name,
            modifiers,
            type_params,
            param_list,
            capability,
            ret_hint,
            readonly_return,
            where_constraints,
        }))
    }

    fn make_yield_expression(&mut self, keyword: Self::R, _operand: Self::R) -> Self::R {
        assert!(keyword.token_kind() == Some(TokenKind::Yield));
        keyword
    }

    fn make_const_declaration(
        &mut self,
        modifiers: Self::R,
        const_keyword: Self::R,
        hint: Self::R,
        decls: Self::R,
        semicolon: Self::R,
    ) -> Self::R {
        match decls {
            // Class consts.
            Node::List(consts)
                if self
                    .classish_name_builder
                    .get_current_classish_name()
                    .is_some() =>
            {
                let ty = self.node_to_ty(hint);
                Node::List(
                    self.alloc(self.slice(consts.iter().filter_map(|cst| match cst {
                        Node::ConstInitializer(&(name, initializer, refs)) => {
                            let id = name.as_id()?;
                            let modifiers = read_member_modifiers(modifiers.iter());
                            let abstract_ = if modifiers.is_abstract {
                                ClassConstKind::CCAbstract(!initializer.is_ignored())
                            } else {
                                ClassConstKind::CCConcrete
                            };
                            let ty = ty
                                .or_else(|| self.infer_const(name, initializer))
                                .unwrap_or_else(|| tany());
                            Some(Node::Const(self.alloc(
                                shallow_decl_defs::ShallowClassConst {
                                    abstract_,
                                    name: id.into(),
                                    type_: ty,
                                    refs,
                                },
                            )))
                        }
                        _ => None,
                    }))),
                )
            }
            // Global consts.
            Node::List(consts) => {
                // This case always returns Node::Ignored,
                // but has the side effect of calling self.add_const

                // Note: given "const int X=1,Y=2;", the legacy decl-parser
                // allows both decls, and it gives them both an identical text-span -
                // from start of "const" to end of semicolon. This is a bug but
                // the code here preserves it.
                let pos = self.merge_positions(const_keyword, semicolon);
                for cst in consts.iter() {
                    match cst {
                        Node::ConstInitializer(&(name, initializer, _refs)) => {
                            if let Some(Id(id_pos, id)) = self.elaborate_defined_id(name) {
                                let ty = self
                                    .node_to_ty(hint)
                                    .or_else(|| self.infer_const(name, initializer))
                                    .unwrap_or_else(|| self.tany_with_pos(id_pos));
                                self.add_const(id, self.alloc(ConstDecl { pos, type_: ty }));
                            }
                        }
                        _ => {}
                    }
                }
                Node::Ignored(SK::ConstDeclaration)
            }
            _ => Node::Ignored(SK::ConstDeclaration),
        }
    }

    fn begin_constant_declarator(&mut self) {
        self.start_accumulating_const_refs();
    }

    fn make_constant_declarator(&mut self, name: Self::R, initializer: Self::R) -> Self::R {
        // The "X=1" part of either a member const "class C {const int X=1;}" or a top-level const "const int X=1;"
        // Note: the the declarator itself doesn't yet know whether a type was provided by the user;
        // that's only known in the parent, make_const_declaration
        let refs = self.stop_accumulating_const_refs();
        if name.is_ignored() {
            Node::Ignored(SK::ConstantDeclarator)
        } else {
            Node::ConstInitializer(self.alloc((name, initializer, refs)))
        }
    }

    fn make_namespace_declaration(&mut self, _name: Self::R, body: Self::R) -> Self::R {
        if let Node::Ignored(SK::NamespaceBody) = body {
            Rc::make_mut(&mut self.namespace_builder).pop_namespace();
        }
        Node::Ignored(SK::NamespaceDeclaration)
    }

    fn make_namespace_declaration_header(&mut self, _keyword: Self::R, name: Self::R) -> Self::R {
        let name = self.expect_name(name).map(|Id(_, name)| name);
        // if this is header of semicolon-style (one with NamespaceEmptyBody) namespace, we should pop
        // the previous namespace first, but we don't have the body yet. We'll fix it retroactively in
        // make_namespace_empty_body
        Rc::make_mut(&mut self.namespace_builder).push_namespace(name);
        Node::Ignored(SK::NamespaceDeclarationHeader)
    }

    fn make_namespace_body(
        &mut self,
        _left_brace: Self::R,
        _declarations: Self::R,
        _right_brace: Self::R,
    ) -> Self::R {
        Node::Ignored(SK::NamespaceBody)
    }

    fn make_namespace_empty_body(&mut self, _semicolon: Self::R) -> Self::R {
        Rc::make_mut(&mut self.namespace_builder).pop_previous_namespace();
        Node::Ignored(SK::NamespaceEmptyBody)
    }

    fn make_namespace_use_declaration(
        &mut self,
        _keyword: Self::R,
        namespace_use_kind: Self::R,
        clauses: Self::R,
        _semicolon: Self::R,
    ) -> Self::R {
        if let Some(import_kind) = Self::namespace_use_kind(&namespace_use_kind) {
            for clause in clauses.iter() {
                if let Node::NamespaceUseClause(nuc) = clause {
                    Rc::make_mut(&mut self.namespace_builder).add_import(
                        import_kind,
                        nuc.id.1,
                        nuc.as_,
                    );
                }
            }
        }
        Node::Ignored(SK::NamespaceUseDeclaration)
    }

    fn make_namespace_group_use_declaration(
        &mut self,
        _keyword: Self::R,
        _kind: Self::R,
        prefix: Self::R,
        _left_brace: Self::R,
        clauses: Self::R,
        _right_brace: Self::R,
        _semicolon: Self::R,
    ) -> Self::R {
        let Id(_, prefix) = match self.expect_name(prefix) {
            Some(id) => id,
            None => return Node::Ignored(SK::NamespaceGroupUseDeclaration),
        };
        for clause in clauses.iter() {
            if let Node::NamespaceUseClause(nuc) = clause {
                let mut id = String::new_in(self.arena);
                id.push_str(prefix);
                id.push_str(nuc.id.1);
                Rc::make_mut(&mut self.namespace_builder).add_import(
                    nuc.kind,
                    id.into_bump_str(),
                    nuc.as_,
                );
            }
        }
        Node::Ignored(SK::NamespaceGroupUseDeclaration)
    }

    fn make_namespace_use_clause(
        &mut self,
        clause_kind: Self::R,
        name: Self::R,
        as_: Self::R,
        aliased_name: Self::R,
    ) -> Self::R {
        let id = match self.expect_name(name) {
            Some(id) => id,
            None => return Node::Ignored(SK::NamespaceUseClause),
        };
        let as_ = if as_.is_token(TokenKind::As) {
            match aliased_name.as_id() {
                Some(name) => Some(name.1),
                None => return Node::Ignored(SK::NamespaceUseClause),
            }
        } else {
            None
        };
        if let Some(kind) = Self::namespace_use_kind(&clause_kind) {
            Node::NamespaceUseClause(self.alloc(NamespaceUseClause { kind, id, as_ }))
        } else {
            Node::Ignored(SK::NamespaceUseClause)
        }
    }

    fn make_where_clause(&mut self, _: Self::R, where_constraints: Self::R) -> Self::R {
        where_constraints
    }

    fn make_where_constraint(
        &mut self,
        left_type: Self::R,
        operator: Self::R,
        right_type: Self::R,
    ) -> Self::R {
        Node::WhereConstraint(self.alloc(WhereConstraint(
            self.node_to_ty(left_type).unwrap_or_else(|| tany()),
            match operator.token_kind() {
                Some(TokenKind::Equal) => ConstraintKind::ConstraintEq,
                Some(TokenKind::Super) => ConstraintKind::ConstraintSuper,
                _ => ConstraintKind::ConstraintAs,
            },
            self.node_to_ty(right_type).unwrap_or_else(|| tany()),
        )))
    }

    fn make_classish_declaration(
        &mut self,
        attributes: Self::R,
        modifiers: Self::R,
        xhp_keyword: Self::R,
        class_keyword: Self::R,
        name: Self::R,
        tparams: Self::R,
        _extends_keyword: Self::R,
        extends: Self::R,
        _implements_keyword: Self::R,
        implements: Self::R,
        where_clause: Self::R,
        body: Self::R,
    ) -> Self::R {
        let raw_name = match self.expect_name(name) {
            Some(Id(_, name)) => name,
            None => return Node::Ignored(SK::ClassishDeclaration),
        };
        let Id(pos, name) = match self.elaborate_defined_id(name) {
            Some(id) => id,
            None => return Node::Ignored(SK::ClassishDeclaration),
        };
        let is_xhp = raw_name.starts_with(':') || xhp_keyword.is_present();

        let mut class_kind = match class_keyword.token_kind() {
            Some(TokenKind::Interface) => ClassishKind::Cinterface,
            Some(TokenKind::Trait) => ClassishKind::Ctrait,
            _ => ClassishKind::Cclass(&Abstraction::Concrete),
        };
        let mut final_ = false;

        for modifier in modifiers.iter() {
            match modifier.token_kind() {
                Some(TokenKind::Abstract) => {
                    class_kind = ClassishKind::Cclass(&Abstraction::Abstract)
                }
                Some(TokenKind::Final) => final_ = true,
                _ => {}
            }
        }

        let where_constraints = self.slice(where_clause.iter().filter_map(|&x| match x {
            Node::WhereConstraint(x) => Some(x),
            _ => None,
        }));

        let body = match body {
            Node::ClassishBody(body) => body,
            _ => return Node::Ignored(SK::ClassishDeclaration),
        };

        let mut uses_len = 0;
        let mut xhp_attr_uses_len = 0;
        let mut xhp_enum_values = SMap::empty();
        let mut req_extends_len = 0;
        let mut req_implements_len = 0;
        let mut consts_len = 0;
        let mut typeconsts_len = 0;
        let mut props_len = 0;
        let mut sprops_len = 0;
        let mut static_methods_len = 0;
        let mut methods_len = 0;

        let mut user_attributes_len = 0;
        for attribute in attributes.iter() {
            match attribute {
                &Node::Attribute(..) => user_attributes_len += 1,
                _ => {}
            }
        }

        for element in body.iter().copied() {
            match element {
                Node::TraitUse(names) => uses_len += names.len(),
                Node::XhpClassAttributeDeclaration(&XhpClassAttributeDeclarationNode {
                    xhp_attr_decls,
                    xhp_attr_uses_decls,
                    xhp_attr_enum_values,
                }) => {
                    props_len += xhp_attr_decls.len();
                    xhp_attr_uses_len += xhp_attr_uses_decls.len();

                    for (name, values) in xhp_attr_enum_values {
                        xhp_enum_values = xhp_enum_values.add(self.arena, name, *values);
                    }
                }
                Node::TypeConstant(..) => typeconsts_len += 1,
                Node::RequireClause(require) => match require.require_type.token_kind() {
                    Some(TokenKind::Extends) => req_extends_len += 1,
                    Some(TokenKind::Implements) => req_implements_len += 1,
                    _ => {}
                },
                Node::List(consts @ [Node::Const(..), ..]) => consts_len += consts.len(),
                Node::Property(&PropertyNode { decls, is_static }) => {
                    if is_static {
                        sprops_len += decls.len()
                    } else {
                        props_len += decls.len()
                    }
                }
                Node::Constructor(&ConstructorNode { properties, .. }) => {
                    props_len += properties.len()
                }
                Node::Method(&MethodNode { is_static, .. }) => {
                    if is_static {
                        static_methods_len += 1
                    } else {
                        methods_len += 1
                    }
                }
                _ => {}
            }
        }

        let mut constructor = None;

        let mut uses = Vec::with_capacity_in(uses_len, self.arena);
        let mut xhp_attr_uses = Vec::with_capacity_in(xhp_attr_uses_len, self.arena);
        let mut req_extends = Vec::with_capacity_in(req_extends_len, self.arena);
        let mut req_implements = Vec::with_capacity_in(req_implements_len, self.arena);
        let mut consts = Vec::with_capacity_in(consts_len, self.arena);
        let mut typeconsts = Vec::with_capacity_in(typeconsts_len, self.arena);
        let mut props = Vec::with_capacity_in(props_len, self.arena);
        let mut sprops = Vec::with_capacity_in(sprops_len, self.arena);
        let mut static_methods = Vec::with_capacity_in(static_methods_len, self.arena);
        let mut methods = Vec::with_capacity_in(methods_len, self.arena);

        let mut user_attributes = Vec::with_capacity_in(user_attributes_len, self.arena);
        for attribute in attributes.iter() {
            match attribute {
                Node::Attribute(attr) => user_attributes.push(self.user_attribute_to_decl(&attr)),
                _ => {}
            }
        }
        // Match ordering of attributes produced by the OCaml decl parser (even
        // though it's the reverse of the syntactic ordering).
        user_attributes.reverse();

        // xhp props go after regular props, regardless of their order in file
        let mut xhp_props = vec![];

        for element in body.iter().copied() {
            match element {
                Node::TraitUse(names) => {
                    uses.extend(names.iter().filter_map(|&name| self.node_to_ty(name)))
                }
                Node::XhpClassAttributeDeclaration(&XhpClassAttributeDeclarationNode {
                    xhp_attr_decls,
                    xhp_attr_uses_decls,
                    ..
                }) => {
                    xhp_props.extend(xhp_attr_decls);
                    xhp_attr_uses.extend(
                        xhp_attr_uses_decls
                            .iter()
                            .filter_map(|&node| self.node_to_ty(node)),
                    )
                }
                Node::TypeConstant(constant) => typeconsts.push(constant),
                Node::RequireClause(require) => match require.require_type.token_kind() {
                    Some(TokenKind::Extends) => {
                        req_extends.extend(self.node_to_ty(require.name).iter())
                    }
                    Some(TokenKind::Implements) => {
                        req_implements.extend(self.node_to_ty(require.name).iter())
                    }
                    _ => {}
                },
                Node::List(&const_nodes @ [Node::Const(..), ..]) => {
                    for node in const_nodes {
                        if let &Node::Const(decl) = node {
                            consts.push(decl)
                        }
                    }
                }
                Node::Property(&PropertyNode { decls, is_static }) => {
                    for property in decls {
                        if is_static {
                            sprops.push(property)
                        } else {
                            props.push(property)
                        }
                    }
                }
                Node::Constructor(&ConstructorNode { method, properties }) => {
                    constructor = Some(method);
                    for property in properties {
                        props.push(property)
                    }
                }
                Node::Method(&MethodNode { method, is_static }) => {
                    if is_static {
                        static_methods.push(method);
                    } else {
                        methods.push(method);
                    }
                }
                _ => {} // It's not our job to report errors here.
            }
        }

        props.extend(xhp_props.into_iter());

        let class_attributes = self.to_attributes(attributes);
        if class_attributes.const_ {
            for prop in props.iter_mut() {
                if !prop.flags.contains(PropFlags::CONST) {
                    *prop = self.alloc(ShallowProp {
                        flags: prop.flags | PropFlags::CONST,
                        ..**prop
                    })
                }
            }
        }

        let uses = uses.into_bump_slice();
        let xhp_attr_uses = xhp_attr_uses.into_bump_slice();
        let req_extends = req_extends.into_bump_slice();
        let req_implements = req_implements.into_bump_slice();
        let consts = consts.into_bump_slice();
        let typeconsts = typeconsts.into_bump_slice();
        let props = props.into_bump_slice();
        let sprops = sprops.into_bump_slice();
        let static_methods = static_methods.into_bump_slice();
        let methods = methods.into_bump_slice();
        let user_attributes = user_attributes.into_bump_slice();
        let extends = self.slice(extends.iter().filter_map(|&node| self.node_to_ty(node)));
        let implements = self.slice(implements.iter().filter_map(|&node| self.node_to_ty(node)));
        let support_dynamic_type =
            self.opts.everything_sdt || class_attributes.support_dynamic_type;
        // Pop the type params stack only after creating all inner types.
        let tparams = self.pop_type_params(tparams);
        let module = class_attributes.module;

        let cls = self.alloc(shallow_decl_defs::ShallowClass {
            mode: self.file_mode,
            final_,
            is_xhp,
            has_xhp_keyword: xhp_keyword.is_token(TokenKind::XHP),
            kind: class_kind,
            module: self.alloc(module),
            name: (pos, name),
            tparams,
            where_constraints,
            extends,
            uses,
            xhp_attr_uses,
            xhp_enum_values,
            req_extends,
            req_implements,
            implements,
            support_dynamic_type,
            consts,
            typeconsts,
            props,
            sprops,
            constructor,
            static_methods,
            methods,
            user_attributes,
            enum_type: None,
        });
        self.add_class(name, cls);

        self.classish_name_builder.parsed_classish_declaration();

        Node::Ignored(SK::ClassishDeclaration)
    }

    fn make_property_declaration(
        &mut self,
        attrs: Self::R,
        modifiers: Self::R,
        hint: Self::R,
        declarators: Self::R,
        _semicolon: Self::R,
    ) -> Self::R {
        let (attrs, modifiers, hint) = (attrs, modifiers, hint);
        let modifiers = read_member_modifiers(modifiers.iter());
        let declarators = self.slice(declarators.iter().filter_map(
            |declarator| match declarator {
                Node::ListItem(&(name, initializer)) => {
                    let attributes = self.to_attributes(attrs);
                    let Id(pos, name) = name.as_variable()?;
                    let name = if modifiers.is_static {
                        name
                    } else {
                        strip_dollar_prefix(name)
                    };
                    let ty = self.node_to_non_ret_ty(hint);
                    let ty = if self.opts.interpret_soft_types_as_like_types {
                        if attributes.soft {
                            ty.map(|t| {
                                self.alloc(Ty(
                                    self.alloc(Reason::hint(self.get_pos(hint))),
                                    Ty_::Tlike(t),
                                ))
                            })
                        } else {
                            ty
                        }
                    } else {
                        ty
                    };
                    let needs_init = if self.file_mode == Mode::Mhhi {
                        false
                    } else {
                        initializer.is_ignored()
                    };
                    let mut flags = PropFlags::empty();
                    flags.set(PropFlags::CONST, attributes.const_);
                    flags.set(PropFlags::LATEINIT, attributes.late_init);
                    flags.set(PropFlags::LSB, attributes.lsb);
                    flags.set(PropFlags::NEEDS_INIT, needs_init);
                    flags.set(PropFlags::ABSTRACT, modifiers.is_abstract);
                    flags.set(PropFlags::READONLY, modifiers.is_readonly);
                    flags.set(PropFlags::PHP_STD_LIB, attributes.php_std_lib);
                    Some(ShallowProp {
                        xhp_attr: None,
                        name: (pos, name),
                        type_: ty,
                        visibility: modifiers.visibility,
                        flags,
                    })
                }
                _ => None,
            },
        ));
        Node::Property(self.alloc(PropertyNode {
            decls: declarators,
            is_static: modifiers.is_static,
        }))
    }

    fn make_xhp_class_attribute_declaration(
        &mut self,
        _keyword: Self::R,
        attributes: Self::R,
        _semicolon: Self::R,
    ) -> Self::R {
        let mut xhp_attr_enum_values = Vec::new_in(self.arena);

        let xhp_attr_decls = self.slice(attributes.iter().filter_map(|node| {
            let node = match node {
                Node::XhpClassAttribute(x) => x,
                _ => return None,
            };
            let Id(pos, name) = node.name;
            let name = prefix_colon(self.arena, name);

            let (type_, enum_values) = match node.hint {
                Node::XhpEnumTy((ty, values)) => (Some(*ty), Some(values)),
                _ => (self.node_to_ty(node.hint), None),
            };
            if let Some(enum_values) = enum_values {
                xhp_attr_enum_values.push((name, *enum_values));
            };

            let type_ = if node.nullable && node.tag.is_none() {
                type_.and_then(|x| match x {
                    // already nullable
                    Ty(_, Ty_::Toption(_)) | Ty(_, Ty_::Tmixed) => type_,
                    // make nullable
                    _ => self.node_to_ty(self.hint_ty(x.get_pos()?, Ty_::Toption(x))),
                })
            } else {
                type_
            };

            let mut flags = PropFlags::empty();
            flags.set(PropFlags::NEEDS_INIT, node.needs_init);
            Some(ShallowProp {
                name: (pos, name),
                visibility: aast::Visibility::Public,
                type_,
                xhp_attr: Some(shallow_decl_defs::XhpAttr {
                    tag: node.tag,
                    has_default: !node.needs_init,
                }),
                flags,
            })
        }));

        let xhp_attr_uses_decls = self.slice(attributes.iter().filter_map(|x| match x {
            Node::XhpAttributeUse(&name) => Some(name),
            _ => None,
        }));

        Node::XhpClassAttributeDeclaration(self.alloc(XhpClassAttributeDeclarationNode {
            xhp_attr_enum_values: xhp_attr_enum_values.into_bump_slice(),
            xhp_attr_decls,
            xhp_attr_uses_decls,
        }))
    }

    /// Handle XHP attribute enum declarations.
    ///
    ///   class :foo implements XHPChild {
    ///     attribute
    ///       enum {'big', 'small'} size; // this line
    ///   }
    fn make_xhp_enum_type(
        &mut self,
        enum_keyword: Self::R,
        _left_brace: Self::R,
        xhp_enum_values: Self::R,
        right_brace: Self::R,
    ) -> Self::R {
        // Infer the type hint from the first value.
        // TODO: T88207956 consider all the values.
        let ty = xhp_enum_values
            .iter()
            .next()
            .and_then(|node| self.node_to_ty(*node))
            .and_then(|node_ty| {
                let pos = self.merge_positions(enum_keyword, right_brace);
                let ty_ = node_ty.1;
                Some(self.alloc(Ty(self.alloc(Reason::hint(pos)), ty_)))
            });

        let mut values = Vec::new_in(self.arena);
        for node in xhp_enum_values.iter() {
            // XHP enum values may only be string or int literals.
            match node {
                Node::IntLiteral(&(s, _)) => {
                    let i = s.parse::<isize>().unwrap_or(0);
                    values.push(XhpEnumValue::XEVInt(i));
                }
                Node::StringLiteral(&(s, _)) => {
                    let owned_str = std::string::String::from_utf8_lossy(s);
                    values.push(XhpEnumValue::XEVString(self.arena.alloc_str(&owned_str)));
                }
                _ => {}
            };
        }

        match ty {
            Some(ty) => Node::XhpEnumTy(self.alloc((&ty, values.into_bump_slice()))),
            None => Node::Ignored(SK::XHPEnumType),
        }
    }

    fn make_xhp_class_attribute(
        &mut self,
        type_: Self::R,
        name: Self::R,
        initializer: Self::R,
        tag: Self::R,
    ) -> Self::R {
        let name = match name.as_id() {
            Some(name) => name,
            None => return Node::Ignored(SK::XHPClassAttribute),
        };
        Node::XhpClassAttribute(self.alloc(XhpClassAttributeNode {
            name,
            hint: type_,
            needs_init: !initializer.is_present(),
            tag: match tag.token_kind() {
                Some(TokenKind::Required) => Some(XhpAttrTag::Required),
                Some(TokenKind::Lateinit) => Some(XhpAttrTag::Lateinit),
                _ => None,
            },
            nullable: initializer.is_token(TokenKind::NullLiteral) || !initializer.is_present(),
        }))
    }

    fn make_xhp_simple_class_attribute(&mut self, name: Self::R) -> Self::R {
        Node::XhpAttributeUse(self.alloc(name))
    }

    fn make_property_declarator(&mut self, name: Self::R, initializer: Self::R) -> Self::R {
        Node::ListItem(self.alloc((name, initializer)))
    }

    fn make_methodish_declaration(
        &mut self,
        attributes: Self::R,
        header: Self::R,
        body: Self::R,
        closer: Self::R,
    ) -> Self::R {
        let header = match header {
            Node::FunctionHeader(header) => header,
            _ => return Node::Ignored(SK::MethodishDeclaration),
        };
        // If we don't have a body, use the closing token. A closing token of
        // '}' indicates a regular function, while a closing token of ';'
        // indicates an abstract function.
        let body = if body.is_ignored() { closer } else { body };
        let modifiers = read_member_modifiers(header.modifiers.iter());
        let is_constructor = header.name.is_token(TokenKind::Construct);
        let is_method = true;
        let (id, ty, properties) = match self.function_to_ty(is_method, attributes, header, body) {
            Some(tuple) => tuple,
            None => return Node::Ignored(SK::MethodishDeclaration),
        };
        let attributes = self.to_attributes(attributes);
        let deprecated = attributes.deprecated.map(|msg| {
            let mut s = String::new_in(self.arena);
            s.push_str("The method ");
            s.push_str(id.1);
            s.push_str(" is deprecated: ");
            s.push_str(msg);
            s.into_bump_str()
        });
        let mut flags = MethodFlags::empty();
        flags.set(
            MethodFlags::ABSTRACT,
            self.classish_name_builder.in_interface() || modifiers.is_abstract,
        );
        flags.set(MethodFlags::FINAL, modifiers.is_final);
        flags.set(MethodFlags::OVERRIDE, attributes.override_);
        flags.set(
            MethodFlags::DYNAMICALLYCALLABLE,
            attributes.dynamically_callable,
        );
        flags.set(MethodFlags::PHP_STD_LIB, attributes.php_std_lib);
        let visibility = match modifiers.visibility {
            aast::Visibility::Public => {
                if attributes.internal {
                    aast::Visibility::Internal
                } else {
                    aast::Visibility::Public
                }
            }
            _ => modifiers.visibility,
        };
        let method = self.alloc(ShallowMethod {
            name: id,
            type_: ty,
            visibility,
            deprecated,
            flags,
        });
        if is_constructor {
            Node::Constructor(self.alloc(ConstructorNode { method, properties }))
        } else {
            Node::Method(self.alloc(MethodNode {
                method,
                is_static: modifiers.is_static,
            }))
        }
    }

    fn make_classish_body(
        &mut self,
        _left_brace: Self::R,
        elements: Self::R,
        _right_brace: Self::R,
    ) -> Self::R {
        Node::ClassishBody(self.alloc(elements.as_slice(self.arena)))
    }

    fn make_enum_declaration(
        &mut self,
        attributes: Self::R,
        _keyword: Self::R,
        name: Self::R,
        _colon: Self::R,
        extends: Self::R,
        constraint: Self::R,
        _left_brace: Self::R,
        use_clauses: Self::R,
        enumerators: Self::R,
        _right_brace: Self::R,
    ) -> Self::R {
        let id = match self.elaborate_defined_id(name) {
            Some(id) => id,
            None => return Node::Ignored(SK::EnumDeclaration),
        };
        let hint = match self.node_to_ty(extends) {
            Some(ty) => ty,
            None => return Node::Ignored(SK::EnumDeclaration),
        };
        let extends = match self.node_to_ty(self.make_apply(
            (self.get_pos(name), "\\HH\\BuiltinEnum"),
            name,
            Pos::none(),
        )) {
            Some(ty) => ty,
            None => return Node::Ignored(SK::EnumDeclaration),
        };
        let key = id.1;
        let consts = self.slice(enumerators.iter().filter_map(|node| match node {
            &Node::Const(const_) => Some(const_),
            _ => None,
        }));
        let mut user_attributes = Vec::with_capacity_in(attributes.len(), self.arena);
        for attribute in attributes.iter() {
            match attribute {
                Node::Attribute(attr) => user_attributes.push(self.user_attribute_to_decl(attr)),
                _ => {}
            }
        }
        // Match ordering of attributes produced by the OCaml decl parser (even
        // though it's the reverse of the syntactic ordering).
        user_attributes.reverse();
        let user_attributes = user_attributes.into_bump_slice();

        let constraint = match constraint {
            Node::TypeConstraint(&(_kind, ty)) => self.node_to_ty(ty),
            _ => None,
        };

        let mut includes_len = 0;
        for element in use_clauses.iter() {
            match element {
                Node::EnumUse(names) => includes_len += names.len(),
                _ => {}
            }
        }
        let mut includes = Vec::with_capacity_in(includes_len, self.arena);
        for element in use_clauses.iter() {
            match element {
                Node::EnumUse(names) => {
                    includes.extend(names.iter().filter_map(|&name| self.node_to_ty(name)))
                }
                _ => {}
            }
        }
        let includes = includes.into_bump_slice();

        let cls = self.alloc(shallow_decl_defs::ShallowClass {
            mode: self.file_mode,
            final_: false,
            is_xhp: false,
            has_xhp_keyword: false,
            kind: ClassishKind::Cenum,
            module: &None, // TODO: grab module from attributes
            name: id.into(),
            tparams: &[],
            where_constraints: &[],
            extends: bumpalo::vec![in self.arena; extends].into_bump_slice(),
            uses: &[],
            xhp_attr_uses: &[],
            xhp_enum_values: SMap::empty(),
            req_extends: &[],
            req_implements: &[],
            implements: &[],
            support_dynamic_type: false,
            consts,
            typeconsts: &[],
            props: &[],
            sprops: &[],
            constructor: None,
            static_methods: &[],
            methods: &[],
            user_attributes,
            enum_type: Some(self.alloc(EnumType {
                base: hint,
                constraint,
                includes,
            })),
        });
        self.add_class(key, cls);

        self.classish_name_builder.parsed_classish_declaration();

        Node::Ignored(SK::EnumDeclaration)
    }

    fn make_enum_use(&mut self, _keyword: Self::R, names: Self::R, _semicolon: Self::R) -> Self::R {
        Node::EnumUse(self.alloc(names))
    }

    fn begin_enumerator(&mut self) {
        self.start_accumulating_const_refs();
    }

    fn make_enumerator(
        &mut self,
        name: Self::R,
        _equal: Self::R,
        value: Self::R,
        _semicolon: Self::R,
    ) -> Self::R {
        let refs = self.stop_accumulating_const_refs();
        let id = match self.expect_name(name) {
            Some(id) => id,
            None => return Node::Ignored(SyntaxKind::Enumerator),
        };

        Node::Const(
            self.alloc(ShallowClassConst {
                abstract_: ClassConstKind::CCConcrete,
                name: id.into(),
                type_: self
                    .infer_const(name, value)
                    .unwrap_or_else(|| self.tany_with_pos(id.0)),
                refs,
            }),
        )
    }

    fn make_enum_class_declaration(
        &mut self,
        attributes: Self::R,
        modifiers: Self::R,
        _enum_keyword: Self::R,
        _class_keyword: Self::R,
        name: Self::R,
        _colon: Self::R,
        base: Self::R,
        _extends_keyword: Self::R,
        extends_list: Self::R,
        _left_brace: Self::R,
        elements: Self::R,
        _right_brace: Self::R,
    ) -> Self::R {
        let name = match self.elaborate_defined_id(name) {
            Some(name) => name,
            None => return Node::Ignored(SyntaxKind::EnumClassDeclaration),
        };
        let base = self
            .node_to_ty(base)
            .unwrap_or_else(|| self.tany_with_pos(name.0));

        let mut is_abstract = false;
        for modifier in modifiers.iter() {
            match modifier.token_kind() {
                Some(TokenKind::Abstract) => is_abstract = true,
                _ => {}
            }
        }

        let class_kind = if is_abstract {
            ClassishKind::CenumClass(&Abstraction::Abstract)
        } else {
            ClassishKind::CenumClass(&Abstraction::Concrete)
        };

        let builtin_enum_class_ty = {
            let pos = name.0;
            let enum_class_ty_ = Ty_::Tapply(self.alloc((name.into(), &[])));
            let enum_class_ty = self.alloc(Ty(self.alloc(Reason::hint(pos)), enum_class_ty_));
            let elt_ty_ = Ty_::Tapply(self.alloc((
                (pos, "\\HH\\MemberOf"),
                bumpalo::vec![in self.arena; enum_class_ty, base].into_bump_slice(),
            )));
            let elt_ty = self.alloc(Ty(self.alloc(Reason::hint(pos)), elt_ty_));
            let builtin_enum_ty_ = if is_abstract {
                Ty_::Tapply(self.alloc(((pos, "\\HH\\BuiltinAbstractEnumClass"), &[])))
            } else {
                Ty_::Tapply(self.alloc((
                    (pos, "\\HH\\BuiltinEnumClass"),
                    std::slice::from_ref(self.alloc(elt_ty)),
                )))
            };
            self.alloc(Ty(self.alloc(Reason::hint(pos)), builtin_enum_ty_))
        };

        let consts = self.slice(elements.iter().filter_map(|node| match node {
            &Node::Const(const_) => Some(const_),
            _ => None,
        }));

        let mut extends = Vec::with_capacity_in(extends_list.len() + 1, self.arena);
        extends.push(builtin_enum_class_ty);
        extends.extend(extends_list.iter().filter_map(|&n| self.node_to_ty(n)));
        let extends = extends.into_bump_slice();
        let includes = &extends[1..];

        let mut user_attributes = Vec::with_capacity_in(attributes.len() + 1, self.arena);
        for attribute in attributes.iter() {
            match attribute {
                Node::Attribute(attr) => user_attributes.push(self.user_attribute_to_decl(attr)),
                _ => {}
            }
        }
        user_attributes.push(self.alloc(shallow_decl_defs::UserAttribute {
            name: (name.0, "__EnumClass"),
            classname_params: &[],
        }));
        // Match ordering of attributes produced by the OCaml decl parser (even
        // though it's the reverse of the syntactic ordering).
        user_attributes.reverse();
        let user_attributes = user_attributes.into_bump_slice();

        let cls = self.alloc(shallow_decl_defs::ShallowClass {
            mode: self.file_mode,
            final_: false,
            is_xhp: false,
            has_xhp_keyword: false,
            kind: class_kind,
            module: &None, // TODO: grab module from attributes
            name: name.into(),
            tparams: &[],
            where_constraints: &[],
            extends,
            uses: &[],
            xhp_attr_uses: &[],
            xhp_enum_values: SMap::empty(),
            req_extends: &[],
            req_implements: &[],
            implements: &[],
            support_dynamic_type: false,
            consts,
            typeconsts: &[],
            props: &[],
            sprops: &[],
            constructor: None,
            static_methods: &[],
            methods: &[],
            user_attributes,
            enum_type: Some(self.alloc(EnumType {
                base,
                constraint: None,
                includes,
            })),
        });
        self.add_class(name.1, cls);

        self.classish_name_builder.parsed_classish_declaration();

        Node::Ignored(SyntaxKind::EnumClassDeclaration)
    }

    fn begin_enum_class_enumerator(&mut self) {
        self.start_accumulating_const_refs();
    }

    fn make_enum_class_enumerator(
        &mut self,
        modifiers: Self::R,
        type_: Self::R,
        name: Self::R,
        _initializer: Self::R,
        _semicolon: Self::R,
    ) -> Self::R {
        let refs = self.stop_accumulating_const_refs();
        let name = match self.expect_name(name) {
            Some(name) => name,
            None => return Node::Ignored(SyntaxKind::EnumClassEnumerator),
        };
        let pos = name.0;
        let has_abstract_keyword = modifiers
            .iter()
            .any(|node| node.is_token(TokenKind::Abstract));
        let abstract_ = if has_abstract_keyword {
            /* default values not allowed atm */
            ClassConstKind::CCAbstract(false)
        } else {
            ClassConstKind::CCConcrete
        };
        let type_ = self
            .node_to_ty(type_)
            .unwrap_or_else(|| self.tany_with_pos(name.0));
        let class_name = match self.classish_name_builder.get_current_classish_name() {
            Some(name) => name,
            None => return Node::Ignored(SyntaxKind::EnumClassEnumerator),
        };
        let enum_class_ty_ = Ty_::Tapply(self.alloc(((pos, class_name.0), &[])));
        let enum_class_ty = self.alloc(Ty(self.alloc(Reason::hint(pos)), enum_class_ty_));
        let type_ = Ty_::Tapply(self.alloc((
            (pos, "\\HH\\MemberOf"),
            bumpalo::vec![in self.arena; enum_class_ty, type_].into_bump_slice(),
        )));
        let type_ = self.alloc(Ty(self.alloc(Reason::hint(pos)), type_));
        Node::Const(self.alloc(ShallowClassConst {
            abstract_,
            name: name.into(),
            type_,
            refs,
        }))
    }

    fn make_tuple_type_specifier(
        &mut self,
        left_paren: Self::R,
        tys: Self::R,
        right_paren: Self::R,
    ) -> Self::R {
        // We don't need to include the tys list in this position merging
        // because by definition it's already contained by the two brackets.
        let pos = self.merge_positions(left_paren, right_paren);
        let tys = self.slice(tys.iter().filter_map(|&node| self.node_to_ty(node)));
        self.hint_ty(pos, Ty_::Ttuple(tys))
    }

    fn make_tuple_type_explicit_specifier(
        &mut self,
        keyword: Self::R,
        _left_angle: Self::R,
        types: Self::R,
        right_angle: Self::R,
    ) -> Self::R {
        let id = (self.get_pos(keyword), "\\tuple");
        // This is an error--tuple syntax is (A, B), not tuple<A, B>.
        // OCaml decl makes a Tapply rather than a Ttuple here.
        self.make_apply(id, types, self.get_pos(right_angle))
    }

    fn make_intersection_type_specifier(
        &mut self,
        left_paren: Self::R,
        tys: Self::R,
        right_paren: Self::R,
    ) -> Self::R {
        let pos = self.merge_positions(left_paren, right_paren);
        let tys = self.slice(tys.iter().filter_map(|x| match x {
            Node::ListItem(&(ty, _ampersand)) => self.node_to_ty(ty),
            &x => self.node_to_ty(x),
        }));
        self.hint_ty(pos, Ty_::Tintersection(tys))
    }

    fn make_union_type_specifier(
        &mut self,
        left_paren: Self::R,
        tys: Self::R,
        right_paren: Self::R,
    ) -> Self::R {
        let pos = self.merge_positions(left_paren, right_paren);
        let tys = self.slice(tys.iter().filter_map(|x| match x {
            Node::ListItem(&(ty, _bar)) => self.node_to_ty(ty),
            &x => self.node_to_ty(x),
        }));
        self.hint_ty(pos, Ty_::Tunion(tys))
    }

    fn make_shape_type_specifier(
        &mut self,
        shape: Self::R,
        _lparen: Self::R,
        fields: Self::R,
        open: Self::R,
        rparen: Self::R,
    ) -> Self::R {
        let fields = fields;
        let fields_iter = fields.iter();
        let mut fields = AssocListMut::new_in(self.arena);
        for node in fields_iter {
            if let &Node::ShapeFieldSpecifier(&ShapeFieldNode { name, type_ }) = node {
                fields.insert(self.make_t_shape_field_name(name), type_)
            }
        }
        let kind = match open.token_kind() {
            Some(TokenKind::DotDotDot) => ShapeKind::OpenShape,
            _ => ShapeKind::ClosedShape,
        };
        let pos = self.merge_positions(shape, rparen);
        self.hint_ty(pos, Ty_::Tshape(self.alloc((kind, fields.into()))))
    }

    fn make_classname_type_specifier(
        &mut self,
        classname: Self::R,
        _lt: Self::R,
        targ: Self::R,
        _trailing_comma: Self::R,
        gt: Self::R,
    ) -> Self::R {
        let id = match classname.as_id() {
            Some(id) => id,
            None => return Node::Ignored(SK::ClassnameTypeSpecifier),
        };
        if gt.is_ignored() {
            self.prim_ty(aast::Tprim::Tstring, id.0)
        } else {
            self.make_apply(
                (id.0, self.elaborate_raw_id(id.1)),
                targ,
                self.merge_positions(classname, gt),
            )
        }
    }

    fn make_scope_resolution_expression(
        &mut self,
        class_name: Self::R,
        _operator: Self::R,
        value: Self::R,
    ) -> Self::R {
        let pos = self.merge_positions(class_name, value);
        let Id(class_name_pos, class_name_str) = match self.expect_name(class_name) {
            Some(id) => self.elaborate_id(id),
            None => return Node::Ignored(SK::ScopeResolutionExpression),
        };
        let class_id = self.alloc(aast::ClassId(
            (),
            class_name_pos,
            match class_name {
                Node::Name(("self", _)) => aast::ClassId_::CIself,
                _ => aast::ClassId_::CI(self.alloc(Id(class_name_pos, class_name_str))),
            },
        ));
        let value_id = match self.expect_name(value) {
            Some(id) => id,
            None => return Node::Ignored(SK::ScopeResolutionExpression),
        };
        self.accumulate_const_ref(class_id, &value_id);
        Node::Expr(self.alloc(aast::Expr(
            (),
            pos,
            nast::Expr_::ClassConst(self.alloc((class_id, self.alloc((value_id.0, value_id.1))))),
        )))
    }

    fn make_field_specifier(
        &mut self,
        question_token: Self::R,
        name: Self::R,
        _arrow: Self::R,
        type_: Self::R,
    ) -> Self::R {
        let optional = question_token.is_present();
        let ty = match self.node_to_ty(type_) {
            Some(ty) => ty,
            None => return Node::Ignored(SK::FieldSpecifier),
        };
        let name = match self.make_shape_field_name(name) {
            Some(name) => name,
            None => return Node::Ignored(SK::FieldSpecifier),
        };
        Node::ShapeFieldSpecifier(self.alloc(ShapeFieldNode {
            name: self.alloc(ShapeField(name)),
            type_: self.alloc(ShapeFieldType { optional, ty }),
        }))
    }

    fn make_field_initializer(&mut self, key: Self::R, _arrow: Self::R, value: Self::R) -> Self::R {
        Node::ListItem(self.alloc((key, value)))
    }

    fn make_varray_type_specifier(
        &mut self,
        varray_keyword: Self::R,
        _less_than: Self::R,
        tparam: Self::R,
        _trailing_comma: Self::R,
        greater_than: Self::R,
    ) -> Self::R {
        let tparam = match self.node_to_ty(tparam) {
            Some(ty) => ty,
            None => self.tany_with_pos(self.get_pos(varray_keyword)),
        };
        self.hint_ty(
            self.merge_positions(varray_keyword, greater_than),
            Ty_::Tapply(self.alloc((
                (
                    self.get_pos(varray_keyword),
                    naming_special_names::collections::VEC,
                ),
                self.alloc([tparam]),
            ))),
        )
    }

    fn make_darray_type_specifier(
        &mut self,
        darray: Self::R,
        _less_than: Self::R,
        key_type: Self::R,
        _comma: Self::R,
        value_type: Self::R,
        _trailing_comma: Self::R,
        greater_than: Self::R,
    ) -> Self::R {
        let pos = self.merge_positions(darray, greater_than);
        let key_type = self.node_to_ty(key_type).unwrap_or(TANY);
        let value_type = self.node_to_ty(value_type).unwrap_or(TANY);
        self.hint_ty(
            pos,
            Ty_::Tapply(self.alloc((
                (
                    self.get_pos(darray),
                    naming_special_names::collections::DICT,
                ),
                self.alloc([key_type, value_type]),
            ))),
        )
    }

    fn make_old_attribute_specification(
        &mut self,
        ltlt: Self::R,
        attrs: Self::R,
        gtgt: Self::R,
    ) -> Self::R {
        match attrs {
            Node::List(nodes) => {
                Node::BracketedList(self.alloc((self.get_pos(ltlt), nodes, self.get_pos(gtgt))))
            }
            _ => Node::Ignored(SK::OldAttributeSpecification),
        }
    }

    fn make_constructor_call(
        &mut self,
        name: Self::R,
        _left_paren: Self::R,
        args: Self::R,
        _right_paren: Self::R,
    ) -> Self::R {
        let unqualified_name = match self.expect_name(name) {
            Some(name) => name,
            None => return Node::Ignored(SK::ConstructorCall),
        };
        let name = if unqualified_name.1.starts_with("__") {
            unqualified_name
        } else {
            match self.expect_name(name) {
                Some(name) => self.elaborate_id(name),
                None => return Node::Ignored(SK::ConstructorCall),
            }
        };
        let classname_params = self.slice(args.iter().filter_map(|node| match node {
            Node::Expr(aast::Expr(
                _,
                full_pos,
                aast::Expr_::ClassConst(&(
                    aast::ClassId(_, _, aast::ClassId_::CI(&Id(pos, class_name))),
                    (_, "class"),
                )),
            )) => {
                let name = self.elaborate_id(Id(pos, class_name));
                Some(ClassNameParam { name, full_pos })
            }
            _ => None,
        }));

        let string_literal_params = if match name.1 {
            "__Deprecated" | "__Cipp" | "__CippLocal" | "__Policied" | "__Module" => true,
            _ => false,
        } {
            fn fold_string_concat<'a>(expr: &nast::Expr<'a>, acc: &mut Vec<'a, u8>) {
                match expr {
                    &aast::Expr(_, _, aast::Expr_::String(val)) => acc.extend_from_slice(val),
                    &aast::Expr(_, _, aast::Expr_::Binop(&(Bop::Dot, e1, e2))) => {
                        fold_string_concat(&e1, acc);
                        fold_string_concat(&e2, acc);
                    }
                    _ => {}
                }
            }

            self.slice(args.iter().filter_map(|expr| match expr {
                Node::StringLiteral((x, _)) => Some(*x),
                Node::Expr(e @ aast::Expr(_, _, aast::Expr_::Binop(_))) => {
                    let mut acc = Vec::new_in(self.arena);
                    fold_string_concat(e, &mut acc);
                    Some(acc.into_bump_slice().into())
                }
                _ => None,
            }))
        } else {
            &[]
        };

        Node::Attribute(self.alloc(UserAttributeNode {
            name,
            classname_params,
            string_literal_params,
        }))
    }

    fn make_trait_use(
        &mut self,
        _keyword: Self::R,
        names: Self::R,
        _semicolon: Self::R,
    ) -> Self::R {
        Node::TraitUse(self.alloc(names))
    }

    fn make_trait_use_conflict_resolution(
        &mut self,
        _keyword: Self::R,
        names: Self::R,
        _left_brace: Self::R,
        _clauses: Self::R,
        _right_brace: Self::R,
    ) -> Self::R {
        Node::TraitUse(self.alloc(names))
    }

    fn make_require_clause(
        &mut self,
        _keyword: Self::R,
        require_type: Self::R,
        name: Self::R,
        _semicolon: Self::R,
    ) -> Self::R {
        Node::RequireClause(self.alloc(RequireClause { require_type, name }))
    }

    fn make_nullable_type_specifier(&mut self, question_mark: Self::R, hint: Self::R) -> Self::R {
        let pos = self.merge_positions(question_mark, hint);
        let ty = match self.node_to_ty(hint) {
            Some(ty) => ty,
            None => return Node::Ignored(SK::NullableTypeSpecifier),
        };
        self.hint_ty(pos, Ty_::Toption(ty))
    }

    fn make_like_type_specifier(&mut self, tilde: Self::R, hint: Self::R) -> Self::R {
        let pos = self.merge_positions(tilde, hint);
        let ty = match self.node_to_ty(hint) {
            Some(ty) => ty,
            None => return Node::Ignored(SK::LikeTypeSpecifier),
        };
        self.hint_ty(pos, Ty_::Tlike(ty))
    }

    fn make_closure_type_specifier(
        &mut self,
        outer_left_paren: Self::R,
        readonly_keyword: Self::R,
        _function_keyword: Self::R,
        _inner_left_paren: Self::R,
        parameter_list: Self::R,
        _inner_right_paren: Self::R,
        capability: Self::R,
        _colon: Self::R,
        readonly_ret: Self::R,
        return_type: Self::R,
        outer_right_paren: Self::R,
    ) -> Self::R {
        let make_param = |fp: &'a FunParamDecl<'a>| -> &'a FunParam<'a> {
            let mut flags = FunParamFlags::empty();

            match fp.kind {
                ParamMode::FPinout => {
                    flags |= FunParamFlags::INOUT;
                }
                ParamMode::FPnormal => {}
            };

            if fp.readonly {
                flags |= FunParamFlags::READONLY;
            }

            self.alloc(FunParam {
                pos: self.get_pos(fp.hint),
                name: None,
                type_: self.alloc(PossiblyEnforcedTy {
                    enforced: Enforcement::Unenforced,
                    type_: self.node_to_ty(fp.hint).unwrap_or_else(|| tany()),
                }),
                flags,
            })
        };

        let arity = parameter_list
            .iter()
            .find_map(|&node| match node {
                Node::FunParam(fp) if fp.variadic => Some(FunArity::Fvariadic(make_param(fp))),
                _ => None,
            })
            .unwrap_or(FunArity::Fstandard);

        let params = self.slice(parameter_list.iter().filter_map(|&node| match node {
            Node::FunParam(fp) if !fp.variadic => Some(make_param(fp)),
            _ => None,
        }));

        let ret = match self.node_to_ty(return_type) {
            Some(ty) => ty,
            None => return Node::Ignored(SK::ClosureTypeSpecifier),
        };
        let pos = self.merge_positions(outer_left_paren, outer_right_paren);
        let implicit_params = self.as_fun_implicit_params(capability, pos);

        let mut flags = FunTypeFlags::empty();
        if readonly_ret.is_token(TokenKind::Readonly) {
            flags |= FunTypeFlags::RETURNS_READONLY;
        }
        if readonly_keyword.is_token(TokenKind::Readonly) {
            flags |= FunTypeFlags::READONLY_THIS;
        }

        self.hint_ty(
            pos,
            Ty_::Tfun(self.alloc(FunType {
                arity,
                tparams: &[],
                where_constraints: &[],
                params,
                implicit_params,
                ret: self.alloc(PossiblyEnforcedTy {
                    enforced: Enforcement::Unenforced,
                    type_: ret,
                }),
                flags,
                ifc_decl: default_ifc_fun_decl(),
            })),
        )
    }

    fn make_closure_parameter_type_specifier(
        &mut self,
        inout: Self::R,
        readonly: Self::R,
        hint: Self::R,
    ) -> Self::R {
        let kind = if inout.is_token(TokenKind::Inout) {
            ParamMode::FPinout
        } else {
            ParamMode::FPnormal
        };
        Node::FunParam(self.alloc(FunParamDecl {
            attributes: Node::Ignored(SK::Missing),
            visibility: Node::Ignored(SK::Missing),
            kind,
            hint,
            readonly: readonly.is_token(TokenKind::Readonly),
            pos: self.get_pos(hint),
            name: Some(""),
            variadic: false,
            initializer: Node::Ignored(SK::Missing),
        }))
    }

    fn make_type_const_declaration(
        &mut self,
        attributes: Self::R,
        modifiers: Self::R,
        _const_keyword: Self::R,
        _type_keyword: Self::R,
        name: Self::R,
        _type_parameters: Self::R,
        as_constraint: Self::R,
        _equal: Self::R,
        type_: Self::R,
        _semicolon: Self::R,
    ) -> Self::R {
        let attributes = self.to_attributes(attributes);
        let has_abstract_keyword = modifiers
            .iter()
            .any(|node| node.is_token(TokenKind::Abstract));
        let as_constraint = match as_constraint {
            Node::TypeConstraint(innards) => self.node_to_ty(innards.1),
            _ => None,
        };
        let type_ = self.node_to_ty(type_);
        let kind = if has_abstract_keyword {
            // Abstract type constant:
            //     abstract const type T [as X] [super Y] [= Z];
            Typeconst::TCAbstract(self.alloc(AbstractTypeconst {
                as_constraint,
                super_constraint: None,
                default: type_,
            }))
        } else {
            if let Some(t) = type_ {
                // Concrete type constant:
                //     const type T = Z;
                Typeconst::TCConcrete(self.alloc(ConcreteTypeconst { tc_type: t }))
            } else {
                // concrete or type constant requires a value
                return Node::Ignored(SK::TypeConstDeclaration);
            }
        };
        let name = match name.as_id() {
            Some(name) => name,
            None => return Node::Ignored(SK::TypeConstDeclaration),
        };
        Node::TypeConstant(self.alloc(ShallowTypeconst {
            name: name.into(),
            kind,
            enforceable: match attributes.enforceable {
                Some(pos) => (pos, true),
                None => (Pos::none(), false),
            },
            reifiable: attributes.reifiable,
            is_ctx: false,
        }))
    }

    fn make_context_const_declaration(
        &mut self,
        modifiers: Self::R,
        _const_keyword: Self::R,
        _ctx_keyword: Self::R,
        name: Self::R,
        _type_parameters: Self::R,
        constraints: Self::R,
        _equal: Self::R,
        ctx_list: Self::R,
        _semicolon: Self::R,
    ) -> Self::R {
        let name = match name.as_id() {
            Some(name) => name,
            None => return Node::Ignored(SK::TypeConstDeclaration),
        };
        let has_abstract_keyword = modifiers
            .iter()
            .any(|node| node.is_token(TokenKind::Abstract));
        let context = self.node_to_ty(ctx_list);

        // note: lowerer ensures that there's at most 1 constraint of each kind
        let mut as_constraint = None;
        let mut super_constraint = None;
        for c in constraints.iter() {
            if let Node::ContextConstraint(&(kind, hint)) = c {
                let ty = self.node_to_ty(hint);
                match kind {
                    ConstraintKind::ConstraintSuper => super_constraint = ty,
                    ConstraintKind::ConstraintAs => as_constraint = ty,
                    _ => {}
                }
            }
        }
        let kind = if has_abstract_keyword {
            Typeconst::TCAbstract(self.alloc(AbstractTypeconst {
                as_constraint,
                super_constraint,
                default: context,
            }))
        } else {
            if let Some(tc_type) = context {
                Typeconst::TCConcrete(self.alloc(ConcreteTypeconst { tc_type }))
            } else {
                /* Concrete type const must have a value */
                return Node::Ignored(SK::TypeConstDeclaration);
            }
        };
        Node::TypeConstant(self.alloc(ShallowTypeconst {
            name: name.into(),
            kind,
            enforceable: (Pos::none(), false),
            reifiable: None,
            is_ctx: true,
        }))
    }

    fn make_decorated_expression(&mut self, decorator: Self::R, expr: Self::R) -> Self::R {
        Node::ListItem(self.alloc((decorator, expr)))
    }

    fn make_type_constant(
        &mut self,
        ty: Self::R,
        _coloncolon: Self::R,
        constant_name: Self::R,
    ) -> Self::R {
        let id = match self.expect_name(constant_name) {
            Some(id) => id,
            None => return Node::Ignored(SK::TypeConstant),
        };
        let pos = self.merge_positions(ty, constant_name);
        let ty = match (ty, self.classish_name_builder.get_current_classish_name()) {
            (Node::Name(("self", self_pos)), Some((name, class_name_pos))) => {
                // In classes, we modify the position when rewriting the
                // `self` keyword to point to the class name. In traits,
                // we don't (because traits are not types). We indicate
                // that the position shouldn't be rewritten with the
                // none Pos.
                let id_pos = if class_name_pos.is_none() {
                    self_pos
                } else {
                    class_name_pos
                };
                let reason = self.alloc(Reason::hint(self_pos));
                let ty_ = Ty_::Tapply(self.alloc(((id_pos, name), &[][..])));
                self.alloc(Ty(reason, ty_))
            }
            _ => match self.node_to_ty(ty) {
                Some(ty) => ty,
                None => return Node::Ignored(SK::TypeConstant),
            },
        };
        let reason = self.alloc(Reason::hint(pos));
        // The reason-rewriting here is only necessary to match the
        // behavior of OCaml decl (which flattens and then unflattens
        // Haccess hints, losing some position information).
        let ty = self.rewrite_taccess_reasons(ty, reason);
        Node::Ty(self.alloc(Ty(
            reason,
            Ty_::Taccess(self.alloc(TaccessType(ty, id.into()))),
        )))
    }

    fn make_soft_type_specifier(&mut self, at_token: Self::R, hint: Self::R) -> Self::R {
        let pos = self.merge_positions(at_token, hint);
        let hint = match self.node_to_ty(hint) {
            Some(ty) => ty,
            None => return Node::Ignored(SK::SoftTypeSpecifier),
        };
        // Use the type of the hint as-is (i.e., throw away the knowledge that
        // we had a soft type specifier here--the typechecker does not use it).
        // Replace its Reason with one including the position of the `@` token.
        self.hint_ty(
            pos,
            if self.opts.interpret_soft_types_as_like_types {
                Ty_::Tlike(hint)
            } else {
                hint.1
            },
        )
    }

    // A type specifier preceded by an attribute list. At the time of writing,
    // only the <<__Soft>> attribute is permitted here.
    fn make_attributized_specifier(&mut self, attributes: Self::R, hint: Self::R) -> Self::R {
        match attributes {
            Node::BracketedList((
                ltlt_pos,
                [Node::Attribute(UserAttributeNode {
                    name: Id(_, "__Soft"),
                    ..
                })],
                gtgt_pos,
            )) => {
                let attributes_pos = self.merge(*ltlt_pos, *gtgt_pos);
                let hint_pos = self.get_pos(hint);
                // Use the type of the hint as-is (i.e., throw away the
                // knowledge that we had a soft type specifier here--the
                // typechecker does not use it). Replace its Reason with one
                // including the position of the attribute list.
                let hint = match self.node_to_ty(hint) {
                    Some(ty) => ty,
                    None => return Node::Ignored(SK::AttributizedSpecifier),
                };

                self.hint_ty(
                    self.merge(attributes_pos, hint_pos),
                    if self.opts.interpret_soft_types_as_like_types {
                        Ty_::Tlike(hint)
                    } else {
                        hint.1
                    },
                )
            }
            _ => hint,
        }
    }

    fn make_vector_type_specifier(
        &mut self,
        vec: Self::R,
        _left_angle: Self::R,
        hint: Self::R,
        _trailing_comma: Self::R,
        right_angle: Self::R,
    ) -> Self::R {
        let id = match self.expect_name(vec) {
            Some(id) => id,
            None => return Node::Ignored(SK::VectorTypeSpecifier),
        };
        let id = (id.0, self.elaborate_raw_id(id.1));
        self.make_apply(id, hint, self.get_pos(right_angle))
    }

    fn make_dictionary_type_specifier(
        &mut self,
        dict: Self::R,
        _left_angle: Self::R,
        type_arguments: Self::R,
        right_angle: Self::R,
    ) -> Self::R {
        let id = match self.expect_name(dict) {
            Some(id) => id,
            None => return Node::Ignored(SK::DictionaryTypeSpecifier),
        };
        let id = (id.0, self.elaborate_raw_id(id.1));
        self.make_apply(id, type_arguments, self.get_pos(right_angle))
    }

    fn make_keyset_type_specifier(
        &mut self,
        keyset: Self::R,
        _left_angle: Self::R,
        hint: Self::R,
        _trailing_comma: Self::R,
        right_angle: Self::R,
    ) -> Self::R {
        let id = match self.expect_name(keyset) {
            Some(id) => id,
            None => return Node::Ignored(SK::KeysetTypeSpecifier),
        };
        let id = (id.0, self.elaborate_raw_id(id.1));
        self.make_apply(id, hint, self.get_pos(right_angle))
    }

    fn make_variable_expression(&mut self, _expression: Self::R) -> Self::R {
        Node::Ignored(SK::VariableExpression)
    }

    fn make_subscript_expression(
        &mut self,
        _receiver: Self::R,
        _left_bracket: Self::R,
        _index: Self::R,
        _right_bracket: Self::R,
    ) -> Self::R {
        Node::Ignored(SK::SubscriptExpression)
    }

    fn make_member_selection_expression(
        &mut self,
        _object: Self::R,
        _operator: Self::R,
        _name: Self::R,
    ) -> Self::R {
        Node::Ignored(SK::MemberSelectionExpression)
    }

    fn make_object_creation_expression(
        &mut self,
        _new_keyword: Self::R,
        _object: Self::R,
    ) -> Self::R {
        Node::Ignored(SK::ObjectCreationExpression)
    }

    fn make_safe_member_selection_expression(
        &mut self,
        _object: Self::R,
        _operator: Self::R,
        _name: Self::R,
    ) -> Self::R {
        Node::Ignored(SK::SafeMemberSelectionExpression)
    }

    fn make_function_call_expression(
        &mut self,
        _receiver: Self::R,
        _type_args: Self::R,
        _enum_class_label: Self::R,
        _left_paren: Self::R,
        _argument_list: Self::R,
        _right_paren: Self::R,
    ) -> Self::R {
        Node::Ignored(SK::FunctionCallExpression)
    }

    fn make_list_expression(
        &mut self,
        _keyword: Self::R,
        _left_paren: Self::R,
        _members: Self::R,
        _right_paren: Self::R,
    ) -> Self::R {
        Node::Ignored(SK::ListExpression)
    }
}
