// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![allow(unused_imports, unused_variables, dead_code)]

use rustc::middle::allocator::AllocatorKind;
use rustc_errors;
use syntax::ast::{Attribute, Crate, LitKind, StrStyle};
use syntax::ast::{Arg, FnHeader, Generics, Mac, Mutability, Ty, Unsafety};
use syntax::ast::{self, Expr, Ident, Item, ItemKind, TyKind, VisibilityKind};
use syntax::attr;
use syntax::codemap::respan;
use syntax::codemap::{ExpnInfo, MacroAttribute};
use syntax::ext::base::ExtCtxt;
use syntax::ext::base::Resolver;
use syntax::ext::build::AstBuilder;
use syntax::ext::expand::ExpansionConfig;
use syntax::ext::hygiene::{self, Mark, SyntaxContext};
use syntax::fold::{self, Folder};
use syntax::parse::ParseSess;
use syntax::ptr::P;
use syntax::symbol::Symbol;
use syntax::util::small_vector::SmallVector;
use syntax_pos::{Span, DUMMY_SP};

use {AllocatorMethod, AllocatorTy, ALLOCATOR_METHODS};

pub fn modify(
    sess: &ParseSess,
    resolver: &mut Resolver,
    krate: Crate,
    crate_name: String,
    handler: &rustc_errors::Handler,
) -> ast::Crate {
    ExpandAllocatorDirectives {
        handler,
        sess,
        resolver,
        found: false,
        crate_name: Some(crate_name),
    }.fold_crate(krate)
}

struct ExpandAllocatorDirectives<'a> {
    found: bool,
    handler: &'a rustc_errors::Handler,
    sess: &'a ParseSess,
    resolver: &'a mut Resolver,
    crate_name: Option<String>,
}

impl<'a> Folder for ExpandAllocatorDirectives<'a> {
    fn fold_item(&mut self, item: P<Item>) -> SmallVector<P<Item>> {
        let name = if attr::contains_name(&item.attrs, "global_allocator") {
            "global_allocator"
        } else {
            return fold::noop_fold_item(item, self);
        };
        match item.node {
            ItemKind::Static(..) => {}
            _ => {
                self.handler
                    .span_err(item.span, "allocators must be statics");
                return SmallVector::one(item);
            }
        }

        if self.found {
            self.handler.span_err(
                item.span,
                "cannot define more than one \
                 #[global_allocator]",
            );
            return SmallVector::one(item);
        }
        self.found = true;

        // Create a fresh Mark for the new macro expansion we are about to do
        let mark = Mark::fresh(Mark::root());
        mark.set_expn_info(ExpnInfo {
            call_site: item.span,
            def_site: None,
            format: MacroAttribute(Symbol::intern(name)),
            allow_internal_unstable: true,
            allow_internal_unsafe: false,
            edition: hygiene::default_edition(),
        });

        // Tie the span to the macro expansion info we just created
        let span = item.span.with_ctxt(SyntaxContext::empty().apply_mark(mark));

        // Create an expansion config
        let ecfg = ExpansionConfig::default(self.crate_name.take().unwrap());

        // Generate a bunch of new items using the AllocFnFactory
        let mut f = AllocFnFactory {
            span,
            kind: AllocatorKind::Global,
            global: item.ident,
            core: Ident::with_empty_ctxt(Symbol::gensym("core")),
            cx: ExtCtxt::new(self.sess, ecfg, self.resolver),
        };

        let extcore = {
            let extcore = f.cx.item_extern_crate(item.span, f.core);
            f.cx.monotonic_expander().fold_item(extcore).pop().unwrap()
        };

        let mut ret = SmallVector::new();
        ret.push(item);
        ret.push(extcore);
        ret.extend(ALLOCATOR_METHODS.iter().map(|method| {
            let method = f.allocator_fn(method);
            f.cx.monotonic_expander().fold_item(method).pop().unwrap()
        }));
        return ret;
    }

    fn fold_mac(&mut self, mac: Mac) -> Mac {
        fold::noop_fold_mac(mac, self)
    }
}

struct AllocFnFactory<'a> {
    span: Span,
    kind: AllocatorKind,
    global: Ident,
    core: Ident,
    cx: ExtCtxt<'a>,
}

impl<'a> AllocFnFactory<'a> {
    fn allocator_fn(&self, method: &AllocatorMethod) -> P<Item> {
        let mut abi_args = Vec::new();
        let mut i = 0;
        let ref mut mk = || {
            let name = Ident::from_str(&format!("arg{}", i));
            i += 1;
            name
        };
        let args = method
            .inputs
            .iter()
            .map(|ty| self.arg_ty(ty, &mut abi_args, mk))
            .collect();
        let result = self.call_allocator(method.name, args);
        let (output_ty, output_expr) = self.ret_ty(&method.output, result);
        let kind = ItemKind::Fn(
            self.cx.fn_decl(abi_args, ast::FunctionRetTy::Ty(output_ty)),
            FnHeader {
                unsafety: Unsafety::Unsafe,
                ..FnHeader::default()
            },
            Generics::default(),
            self.cx.block_expr(output_expr),
        );
        self.cx.item(
            self.span,
            Ident::from_str(&self.kind.fn_name(method.name)),
            self.attrs(),
            kind,
        )
    }

    fn call_allocator(&self, method: &str, mut args: Vec<P<Expr>>) -> P<Expr> {
        let method = self.cx.path(
            self.span,
            vec![
                Ident::from_str("self"),
                self.core,
                Ident::from_str("alloc"),
                Ident::from_str("GlobalAlloc"),
                Ident::from_str(method),
            ],
        );
        let method = self.cx.expr_path(method);
        let allocator = self.cx.path_ident(self.span, self.global);
        let allocator = self.cx.expr_path(allocator);
        let allocator = self.cx.expr_addr_of(self.span, allocator);
        args.insert(0, allocator);

        self.cx.expr_call(self.span, method, args)
    }

    fn attrs(&self) -> Vec<Attribute> {
        let key = Symbol::intern("linkage");
        let value = LitKind::Str(Symbol::intern("external"), StrStyle::Cooked);
        let linkage = self.cx.meta_name_value(self.span, key, value);

        let no_mangle = Symbol::intern("no_mangle");
        let no_mangle = self.cx.meta_word(self.span, no_mangle);

        let special = Symbol::intern("rustc_std_internal_symbol");
        let special = self.cx.meta_word(self.span, special);
        vec![
            self.cx.attribute(self.span, linkage),
            self.cx.attribute(self.span, no_mangle),
            self.cx.attribute(self.span, special),
        ]
    }

    fn arg_ty(
        &self,
        ty: &AllocatorTy,
        args: &mut Vec<Arg>,
        ident: &mut FnMut() -> Ident,
    ) -> P<Expr> {
        match *ty {
            AllocatorTy::Layout => {
                let usize = self.cx.path_ident(self.span, Ident::from_str("usize"));
                let ty_usize = self.cx.ty_path(usize);
                let size = ident();
                let align = ident();
                args.push(self.cx.arg(self.span, size, ty_usize.clone()));
                args.push(self.cx.arg(self.span, align, ty_usize));

                let layout_new = self.cx.path(
                    self.span,
                    vec![
                        Ident::from_str("self"),
                        self.core,
                        Ident::from_str("alloc"),
                        Ident::from_str("Layout"),
                        Ident::from_str("from_size_align_unchecked"),
                    ],
                );
                let layout_new = self.cx.expr_path(layout_new);
                let size = self.cx.expr_ident(self.span, size);
                let align = self.cx.expr_ident(self.span, align);
                let layout = self.cx.expr_call(self.span, layout_new, vec![size, align]);
                layout
            }

            AllocatorTy::Ptr => {
                let ident = ident();
                args.push(self.cx.arg(self.span, ident, self.ptr_u8()));
                let arg = self.cx.expr_ident(self.span, ident);
                self.cx.expr_cast(self.span, arg, self.ptr_u8())
            }

            AllocatorTy::Usize => {
                let ident = ident();
                args.push(self.cx.arg(self.span, ident, self.usize()));
                self.cx.expr_ident(self.span, ident)
            }

            AllocatorTy::ResultPtr | AllocatorTy::Unit => {
                panic!("can't convert AllocatorTy to an argument")
            }
        }
    }

    fn ret_ty(&self, ty: &AllocatorTy, expr: P<Expr>) -> (P<Ty>, P<Expr>) {
        match *ty {
            AllocatorTy::ResultPtr => {
                // We're creating:
                //
                //      #expr as *mut u8

                let expr = self.cx.expr_cast(self.span, expr, self.ptr_u8());
                (self.ptr_u8(), expr)
            }

            AllocatorTy::Unit => (self.cx.ty(self.span, TyKind::Tup(Vec::new())), expr),

            AllocatorTy::Layout | AllocatorTy::Usize | AllocatorTy::Ptr => {
                panic!("can't convert AllocatorTy to an output")
            }
        }
    }

    fn usize(&self) -> P<Ty> {
        let usize = self.cx.path_ident(self.span, Ident::from_str("usize"));
        self.cx.ty_path(usize)
    }

    fn ptr_u8(&self) -> P<Ty> {
        let u8 = self.cx.path_ident(self.span, Ident::from_str("u8"));
        let ty_u8 = self.cx.ty_path(u8);
        self.cx.ty_ptr(self.span, ty_u8, Mutability::Mutable)
    }
}
