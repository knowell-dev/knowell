//! Per-language extraction: symbols (names, qualified names, kinds, ranges,
//! signatures, docs, visibility), imports, chunks and skeletons.

mod common;

use common::*;
use knowell_parse::{ChunkKind, ChunkOptions, Language, SymbolKind as K, Tier, Visibility};
use pretty_assertions::assert_eq;

const RUST: &str = r#"//! Subscription handling.
use std::collections::HashMap;
use crate::billing::{Invoice, Plan};

/// Maximum retries.
pub const MAX_RETRIES: u32 = 3;

/// A customer subscription.
#[derive(Debug, Clone)]
pub struct Subscription {
    pub id: u64,
    plan: Plan,
}

pub enum Status { Active, Cancelled }

pub trait Store {
    /// Loads one.
    fn load(&self, id: u64) -> Option<Subscription>;
}

pub type Index = HashMap<u64, Subscription>;

impl Subscription {
    /// Cancels the subscription.
    pub fn cancel(&mut self, reason: &str) -> Result<(), Error> {
        let x = 1;
        Ok(())
    }

    fn helper(&self) {}
}

impl Store for Memory {
    fn load(&self, id: u64) -> Option<Subscription> { None }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() { assert!(true); }
}

macro_rules! hello { () => {}; }
"#;

#[test]
fn rust() {
    let file = parsed("src/billing.rs", RUST);
    assert_eq!((file.language, file.tier), (Language::Rust, Tier::Exact));
    assert!(!file.has_errors && file.degraded.is_none() && !file.is_generated);
    expect_symbols(
        &file,
        &[
            (K::Constant, "MAX_RETRIES", 5, 6),
            (K::Struct, "Subscription", 8, 13),
            (K::Field, "Subscription.id", 11, 11),
            (K::Field, "Subscription.plan", 12, 12),
            (K::Enum, "Status", 15, 15),
            (K::Trait, "Store", 17, 20),
            (K::Method, "Store.load", 18, 19),
            (K::TypeAlias, "Index", 22, 22),
            (K::Impl, "Subscription", 24, 32),
            (K::Method, "Subscription.cancel", 25, 29),
            (K::Method, "Subscription.helper", 31, 31),
            (K::Impl, "Memory", 34, 36),
            (K::Method, "Memory.load", 35, 35),
            (K::Module, "tests", 38, 42),
            (K::Function, "tests.it_works", 40, 41),
            (K::Macro, "hello", 44, 44),
        ],
    );
    assert_eq!(file.symbols.len(), 16, "{:#?}", summary(&file));
    let cancel = symbol(&file, K::Method, "Subscription.cancel");
    assert_eq!(cancel.name, "cancel");
    assert_eq!(cancel.name_line, 26);
    assert_eq!(
        cancel.signature,
        "pub fn cancel(&mut self, reason: &str) -> Result<(), Error>"
    );
    assert_eq!(cancel.doc.as_deref(), Some("Cancels the subscription."));
    assert_eq!(cancel.visibility, Some(Visibility::Public));
    assert!(cancel.has_body);
    let impl_index = file.symbols.iter().position(|s| s.kind == K::Impl).unwrap();
    assert_eq!(cancel.parent, Some(impl_index));
    assert_eq!(
        symbol(&file, K::Method, "Subscription.helper").visibility,
        Some(Visibility::Private)
    );
    // Trait-impl items are as visible as the trait.
    assert_eq!(
        symbol(&file, K::Method, "Memory.load").visibility,
        Some(Visibility::Public)
    );
    assert_eq!(
        symbol(&file, K::Impl, "Memory").signature,
        "impl Store for Memory"
    );
    let store_load = symbol(&file, K::Method, "Store.load");
    assert!(!store_load.has_body);
    assert_eq!(store_load.doc.as_deref(), Some("Loads one."));
    let subscription = symbol(&file, K::Struct, "Subscription");
    assert_eq!(
        subscription.doc.as_deref(),
        Some("A customer subscription.")
    );
    assert_eq!(subscription.signature, "pub struct Subscription");
    assert_eq!(
        imports(&file),
        [
            "std::collections::HashMap",
            "crate::billing::{Invoice, Plan}"
        ]
    );
    assert_eq!(file.imports[1].range.start(), 3);

    let options = ChunkOptions {
        target_chars: 300,
        min_chars: 30,
        overlap_chars: 0,
    };
    let list = chunked(&file, RUST, &options);
    assert_covers(RUST, &list);
    assert_eq!(list[0].kind, ChunkKind::TopLevel);
    assert!(list[0].text.contains("use std::collections::HashMap;"));
    assert_eq!(chunk_for(&list, "Subscription").kind, ChunkKind::Class);
    assert_eq!(chunk_for(&list, "Store").kind, ChunkKind::Interface);
    assert_eq!(chunk_for(&list, "tests").kind, ChunkKind::Test);
    assert_eq!(chunk_for(&list, "hello").kind, ChunkKind::Declaration);
    assert!(
        chunk_for(&list, "MAX_RETRIES")
            .text
            .starts_with("/// Maximum retries.")
    );

    let skeleton = skeleton_of(&file, RUST);
    assert!(skeleton.contains(
        "impl Subscription {\n    /// Cancels the subscription.\n    pub fn cancel(&mut self, reason: &str) -> Result<(), Error> { … }\n    fn helper(&self) { … }\n}\n"
    ), "{skeleton}");
    assert!(skeleton.contains("pub trait Store {\n    /// Loads one.\n    fn load(&self, id: u64) -> Option<Subscription>;\n}\n"));
    assert!(!skeleton.contains("let x = 1"));
}

const TYPESCRIPT: &str = r#"import { Injectable } from '@nestjs/common';
import * as path from "path";
import type { Plan } from './plan';
export { helper } from './helper';

/** Subscription service. */
@Injectable()
export class SubscriptionService {
  private readonly cache = new Map<string, Plan>();

  constructor(private repo: Repo) {}

  /** Cancels a subscription. */
  async cancelSubscription(id: string): Promise<void> {
    await this.repo.delete(id);
  }

  get size(): number { return 1; }
}

export interface Plan {
  id: string;
  price(): number;
}

export type Id = string | number;

export enum Color { Red, Green }

export function topLevel(a: number): number {
  return a * 2;
}

const arrow = (x: number) => x + 1;
export const handler = async (req: Request): Promise<Response> => {
  return new Response();
};

namespace Utils {
  export function inner() {}
}

abstract class Base {
  abstract run(): void;
}
"#;

#[test]
fn typescript() {
    let file = parsed("src/billing/service.ts", TYPESCRIPT);
    assert_eq!(
        (file.language, file.tier),
        (Language::TypeScript, Tier::Exact)
    );
    expect_symbols(
        &file,
        &[
            (K::Class, "SubscriptionService", 6, 19),
            (K::Field, "SubscriptionService.cache", 9, 9),
            (K::Constructor, "SubscriptionService.constructor", 11, 11),
            (K::Method, "SubscriptionService.cancelSubscription", 13, 16),
            (K::Method, "SubscriptionService.size", 18, 18),
            (K::Interface, "Plan", 21, 24),
            (K::Field, "Plan.id", 22, 22),
            (K::Method, "Plan.price", 23, 23),
            (K::TypeAlias, "Id", 26, 26),
            (K::Enum, "Color", 28, 28),
            (K::Function, "topLevel", 30, 32),
            (K::Function, "arrow", 34, 34),
            (K::Function, "handler", 35, 37),
            (K::Module, "Utils", 39, 41),
            (K::Function, "Utils.inner", 40, 40),
            (K::Class, "Base", 43, 45),
            (K::Method, "Base.run", 44, 44),
        ],
    );
    assert_eq!(file.symbols.len(), 17, "{:#?}", summary(&file));
    let service = symbol(&file, K::Class, "SubscriptionService");
    assert_eq!(
        service.signature,
        "@Injectable()\nexport class SubscriptionService"
    );
    assert_eq!(service.doc.as_deref(), Some("Subscription service."));
    assert_eq!(service.visibility, Some(Visibility::Public));
    assert_eq!(service.name_line, 8);
    let cancel = symbol(&file, K::Method, "SubscriptionService.cancelSubscription");
    assert_eq!(
        cancel.signature,
        "async cancelSubscription(id: string): Promise<void>"
    );
    assert_eq!(cancel.doc.as_deref(), Some("Cancels a subscription."));
    assert_eq!(
        symbol(&file, K::Field, "SubscriptionService.cache").visibility,
        Some(Visibility::Private)
    );
    assert_eq!(
        symbol(&file, K::Function, "arrow").visibility,
        Some(Visibility::Private)
    );
    assert_eq!(
        symbol(&file, K::Function, "handler").signature,
        "export const handler = async (req: Request): Promise<Response> =>"
    );
    assert_eq!(
        imports(&file),
        ["@nestjs/common", "path", "./plan", "./helper"]
    );

    let list = chunked(&file, TYPESCRIPT, &options(256));
    assert_covers(TYPESCRIPT, &list);
    // The class does not fit: a header chunk plus member chunks pointing at it.
    let header = chunk_for(&list, "SubscriptionService");
    assert_eq!(header.kind, ChunkKind::Class);
    assert!(header.text.contains("{ … }"), "{}", header.text);
    let method = chunk_for(&list, "SubscriptionService.cancelSubscription");
    assert_eq!(method.kind, ChunkKind::Method);
    assert_eq!(method.parent, Some(header.ordinal));
    assert!(method.text.starts_with("/** Cancels a subscription. */"));

    let skeleton = skeleton_of(&file, TYPESCRIPT);
    assert!(skeleton.contains("    // Cancels a subscription.\n    async cancelSubscription(id: string): Promise<void> { … }\n"), "{skeleton}");
    assert!(skeleton.contains("export interface Plan {\n    id: string\n    price(): number\n}\n"));
    assert!(skeleton.contains("const arrow = (x: number) => { … }\n"));
}

const TSX: &str = r#"import React from 'react';

export function Button({ label }: Props) {
  return <button>{label}</button>;
}

export const Card = ({ title }: { title: string }) => <div>{title}</div>;
"#;

#[test]
fn tsx() {
    let file = parsed("web/Button.tsx", TSX);
    assert_eq!(file.language, Language::Tsx);
    assert!(!file.has_errors);
    expect_symbols(
        &file,
        &[(K::Function, "Button", 3, 5), (K::Function, "Card", 7, 7)],
    );
    // Inline object types are not members.
    assert_eq!(qualified_names(&file), ["Button", "Card"]);
    assert_eq!(imports(&file), ["react"]);
}

const JAVASCRIPT: &str = r#"const express = require('express');
import { a } from "./a.js";

/**
 * Creates the app.
 */
function createApp(options) {
  return express();
}

class Router extends Base {
  #secret = 1;
  static create() { return new Router(); }
  handle(req, res) {
    res.send('ok');
  }
}

export default function main() {}

const util = function named() {};
module.exports = { createApp };
const lazy = () => import('./lazy.js');
"#;

#[test]
fn javascript() {
    let file = parsed("src/app.js", JAVASCRIPT);
    assert_eq!(file.language, Language::JavaScript);
    expect_symbols(
        &file,
        &[
            (K::Function, "createApp", 4, 9),
            (K::Class, "Router", 11, 17),
            (K::Field, "Router.#secret", 12, 12),
            (K::Method, "Router.create", 13, 13),
            (K::Method, "Router.handle", 14, 16),
            (K::Function, "main", 19, 19),
            (K::Function, "util", 21, 21),
            (K::Function, "lazy", 23, 23),
        ],
    );
    // `const express = require(…)` is an import, not a constant.
    assert_eq!(file.symbols.len(), 8, "{:#?}", summary(&file));
    assert_eq!(
        symbol(&file, K::Function, "createApp").doc.as_deref(),
        Some("Creates the app.")
    );
    assert_eq!(
        symbol(&file, K::Field, "Router.#secret").visibility,
        Some(Visibility::Private)
    );
    // CommonJS exports are assignments: visibility is not claimed.
    assert_eq!(symbol(&file, K::Function, "createApp").visibility, None);
    assert_eq!(
        symbol(&file, K::Function, "main").visibility,
        Some(Visibility::Public)
    );
    assert_eq!(imports(&file), ["express", "./a.js", "./lazy.js"]);
    assert_eq!(file.imports[2].range.start(), 23);
    let list = chunked(&file, JAVASCRIPT, &ChunkOptions::default());
    assert_covers(JAVASCRIPT, &list);
}

#[test]
fn javascript_test_blocks() {
    let text = r#"import { cancel } from '../src/billing';

describe('billing', () => {
  it('cancels a subscription', async () => {
    expect(await cancel('s1')).toBe(true);
  });

  it.skip('refunds', () => {});
});
"#;
    let file = parsed("web/__tests__/billing.test.ts", text);
    expect_symbols(
        &file,
        &[
            (K::Test, "billing", 3, 9),
            (K::Test, "billing > cancels a subscription", 4, 6),
            (K::Test, "billing > refunds", 8, 8),
        ],
    );
    let list = chunked(&file, text, &options(256));
    assert!(list.iter().all(|c| c.kind != ChunkKind::Function));
    assert!(
        list.iter()
            .any(|c| c.kind == ChunkKind::Test || c.kind == ChunkKind::File)
    );
}

const PYTHON: &str = r#""""Billing module."""
import os
from typing import Optional
from .models import Plan as P

MAX = 10

def top(a: int, b: int = 2) -> int:
    """Adds things."""
    return a + b

@dataclass
class Invoice(Base):
    """An invoice."""

    total: int = 0

    def pay(self, amount: int) -> None:
        """Pays the invoice."""
        self.total -= amount

    async def _refresh(self):
        pass

    class Meta:
        ordering = ["id"]
"#;

#[test]
fn python() {
    let file = parsed("billing/invoice.py", PYTHON);
    assert_eq!((file.language, file.tier), (Language::Python, Tier::Exact));
    expect_symbols(
        &file,
        &[
            (K::Constant, "MAX", 6, 6),
            (K::Function, "top", 8, 10),
            (K::Class, "Invoice", 12, 26),
            (K::Field, "Invoice.total", 16, 16),
            (K::Method, "Invoice.pay", 18, 20),
            (K::Method, "Invoice._refresh", 22, 23),
            (K::Class, "Invoice.Meta", 25, 26),
            (K::Field, "Invoice.Meta.ordering", 26, 26),
        ],
    );
    assert_eq!(file.symbols.len(), 8);
    let top = symbol(&file, K::Function, "top");
    assert_eq!(top.signature, "def top(a: int, b: int = 2) -> int");
    assert_eq!(top.doc.as_deref(), Some("Adds things."));
    let invoice = symbol(&file, K::Class, "Invoice");
    assert_eq!(invoice.signature, "@dataclass\nclass Invoice(Base)");
    assert_eq!(invoice.name_line, 13);
    assert_eq!(
        symbol(&file, K::Method, "Invoice._refresh").visibility,
        Some(Visibility::Private)
    );
    assert_eq!(imports(&file), ["os", "typing", ".models"]);
    let skeleton = skeleton_of(&file, PYTHON);
    assert_eq!(
        skeleton,
        "MAX = 10\n\
         def top(a: int, b: int = 2) -> int:\n    \"\"\"Adds things.\"\"\"\n\
         @dataclass\nclass Invoice(Base):\n    \"\"\"An invoice.\"\"\"\n    total: int = 0\n\
         \x20   def pay(self, amount: int) -> None:\n        \"\"\"Pays the invoice.\"\"\"\n\
         \x20   async def _refresh(self):\n        ...\n\
         \x20   class Meta:\n        ordering = [\"id\"]\n"
    );
    let list = chunked(&file, PYTHON, &options(256));
    assert_covers(PYTHON, &list);
}

const GO: &str = r#"// Package billing does billing.
package billing

import (
	"context"
	f "fmt"
)

import "strings"

// MaxRetries is the max.
const MaxRetries = 3

var defaultPlan = "free"

// Service handles subscriptions.
type Service struct {
	repo Repo
}

// Repo stores things.
type Repo interface {
	Load(ctx context.Context, id string) (*Sub, error)
}

type ID = string

// Cancel cancels a subscription.
func (s *Service) Cancel(ctx context.Context, id string) error {
	return nil
}

func helper() {}
"#;

#[test]
fn go() {
    let file = parsed("billing/service.go", GO);
    assert_eq!(file.language, Language::Go);
    expect_symbols(
        &file,
        &[
            (K::Constant, "MaxRetries", 11, 12),
            (K::Variable, "defaultPlan", 14, 14),
            (K::Struct, "Service", 16, 19),
            (K::Field, "Service.repo", 18, 18),
            (K::Interface, "Repo", 21, 24),
            (K::Method, "Repo.Load", 23, 23),
            (K::TypeAlias, "ID", 26, 26),
            (K::Method, "Service.Cancel", 28, 31),
            (K::Function, "helper", 33, 33),
        ],
    );
    assert_eq!(file.symbols.len(), 9);
    let cancel = symbol(&file, K::Method, "Service.Cancel");
    assert_eq!(
        cancel.signature,
        "func (s *Service) Cancel(ctx context.Context, id string) error"
    );
    assert_eq!(
        cancel.doc.as_deref(),
        Some("Cancel cancels a subscription.")
    );
    assert_eq!(cancel.visibility, Some(Visibility::Public));
    assert_eq!(
        symbol(&file, K::Function, "helper").visibility,
        Some(Visibility::Private)
    );
    assert_eq!(
        symbol(&file, K::Interface, "Repo").signature,
        "type Repo interface"
    );
    assert_eq!(imports(&file), ["context", "fmt", "strings"]);
    let skeleton = skeleton_of(&file, GO);
    assert!(skeleton.contains("// Repo stores things.\ntype Repo interface {\n    Load(ctx context.Context, id string) (*Sub, error)\n}\n"), "{skeleton}");
    assert!(
        skeleton.contains("func (s *Service) Cancel(ctx context.Context, id string) error { … }\n")
    );
}

const JAVA: &str = r#"package com.example.billing;

import java.util.List;
import static java.util.Objects.requireNonNull;

/** Subscription service. */
@Service
public class SubscriptionService implements Cancellable {
    private static final int MAX = 3;
    private final Repo repo;

    public SubscriptionService(Repo repo) { this.repo = repo; }

    /** Cancels it. */
    @Override
    public void cancelSubscription(String id) throws IOException {
        repo.delete(id);
    }

    void packagePrivate() {}

    enum Status { ACTIVE, CANCELLED }
}

interface Cancellable {
    void cancelSubscription(String id) throws IOException;
}

record Point(int x, int y) {}
"#;

#[test]
fn java() {
    let file = parsed(
        "src/main/java/com/example/billing/SubscriptionService.java",
        JAVA,
    );
    assert_eq!(file.language, Language::Java);
    expect_symbols(
        &file,
        &[
            (K::Class, "SubscriptionService", 6, 23),
            (K::Field, "SubscriptionService.MAX", 9, 9),
            (K::Field, "SubscriptionService.repo", 10, 10),
            (
                K::Constructor,
                "SubscriptionService.SubscriptionService",
                12,
                12,
            ),
            (K::Method, "SubscriptionService.cancelSubscription", 14, 18),
            (K::Method, "SubscriptionService.packagePrivate", 20, 20),
            (K::Enum, "SubscriptionService.Status", 22, 22),
            (K::Interface, "Cancellable", 25, 27),
            (K::Method, "Cancellable.cancelSubscription", 26, 26),
            (K::Class, "Point", 29, 29),
        ],
    );
    let cancel = symbol(&file, K::Method, "SubscriptionService.cancelSubscription");
    assert_eq!(
        cancel.signature,
        "@Override\npublic void cancelSubscription(String id) throws IOException"
    );
    assert_eq!(cancel.doc.as_deref(), Some("Cancels it."));
    assert_eq!(
        symbol(&file, K::Method, "SubscriptionService.packagePrivate").visibility,
        Some(Visibility::Internal)
    );
    assert_eq!(
        symbol(&file, K::Method, "Cancellable.cancelSubscription").visibility,
        Some(Visibility::Public)
    );
    assert_eq!(
        imports(&file),
        ["java.util.List", "java.util.Objects.requireNonNull"]
    );
}

const KOTLIN: &str = r#"package com.example.billing

import kotlinx.coroutines.flow.Flow
import java.util.UUID as Id

/** Subscription service. */
class SubscriptionService(private val repo: Repo) : Base() {
    private val cache = mutableMapOf<String, Int>()

    /** Cancels it. */
    suspend fun cancelSubscription(id: String): Boolean {
        return repo.delete(id)
    }

    internal fun helper() = 1

    companion object {
        const val MAX = 3
    }
}

interface Repo {
    fun delete(id: String): Boolean
}

data class Plan(val id: String)

object Registry {
    fun register() {}
}

enum class Status { ACTIVE, CANCELLED }

typealias PlanId = String

fun topLevel(x: Int): Int = x * 2

val version = "1"
"#;

#[test]
fn kotlin() {
    let file = parsed("app/src/main/kotlin/SubscriptionService.kt", KOTLIN);
    assert_eq!(file.language, Language::Kotlin);
    expect_symbols(
        &file,
        &[
            (K::Class, "SubscriptionService", 6, 20),
            (K::Field, "SubscriptionService.cache", 8, 8),
            (K::Method, "SubscriptionService.cancelSubscription", 10, 13),
            (K::Method, "SubscriptionService.helper", 15, 15),
            (K::Field, "SubscriptionService.MAX", 18, 18),
            (K::Interface, "Repo", 22, 24),
            (K::Method, "Repo.delete", 23, 23),
            (K::Class, "Plan", 26, 26),
            (K::Class, "Registry", 28, 30),
            (K::Method, "Registry.register", 29, 29),
            (K::Enum, "Status", 32, 32),
            (K::TypeAlias, "PlanId", 34, 34),
            (K::Function, "topLevel", 36, 36),
            (K::Variable, "version", 38, 38),
        ],
    );
    assert_eq!(
        symbol(&file, K::Method, "SubscriptionService.helper").visibility,
        Some(Visibility::Internal)
    );
    assert_eq!(
        symbol(&file, K::Function, "topLevel").signature,
        "fun topLevel(x: Int): Int"
    );
    assert_eq!(
        imports(&file),
        ["kotlinx.coroutines.flow.Flow", "java.util.UUID"]
    );
}

const CSHARP: &str = r#"using System;
using System.Collections.Generic;
using static System.Math;

namespace Billing.Services
{
    /// <summary>Subscription service.</summary>
    public class SubscriptionService : ISubscriptionService
    {
        private readonly IRepo _repo;
        public int Count { get; set; }

        public SubscriptionService(IRepo repo) { _repo = repo; }

        /// <summary>Cancels it.</summary>
        public async Task CancelSubscriptionAsync(string id)
        {
            await _repo.DeleteAsync(id);
        }

        private void Helper() {}
    }

    public interface ISubscriptionService
    {
        Task CancelSubscriptionAsync(string id);
    }

    public record Plan(string Id);
    public struct Point { public int X; }
    public enum Status { Active, Cancelled }
}
"#;

#[test]
fn csharp() {
    let file = parsed("src/Billing/SubscriptionService.cs", CSHARP);
    assert_eq!(file.language, Language::CSharp);
    expect_symbols(
        &file,
        &[
            (K::Module, "Billing.Services", 5, 32),
            (K::Class, "Billing.Services.SubscriptionService", 7, 22),
            (
                K::Field,
                "Billing.Services.SubscriptionService._repo",
                10,
                10,
            ),
            (
                K::Field,
                "Billing.Services.SubscriptionService.Count",
                11,
                11,
            ),
            (
                K::Constructor,
                "Billing.Services.SubscriptionService.SubscriptionService",
                13,
                13,
            ),
            (
                K::Method,
                "Billing.Services.SubscriptionService.CancelSubscriptionAsync",
                15,
                19,
            ),
            (
                K::Method,
                "Billing.Services.SubscriptionService.Helper",
                21,
                21,
            ),
            (
                K::Interface,
                "Billing.Services.ISubscriptionService",
                24,
                27,
            ),
            (
                K::Method,
                "Billing.Services.ISubscriptionService.CancelSubscriptionAsync",
                26,
                26,
            ),
            (K::Class, "Billing.Services.Plan", 29, 29),
            (K::Struct, "Billing.Services.Point", 30, 30),
            (K::Field, "Billing.Services.Point.X", 30, 30),
            (K::Enum, "Billing.Services.Status", 31, 31),
        ],
    );
    let cancel = symbol(
        &file,
        K::Method,
        "Billing.Services.SubscriptionService.CancelSubscriptionAsync",
    );
    assert_eq!(cancel.doc.as_deref(), Some("Cancels it."));
    assert_eq!(cancel.visibility, Some(Visibility::Public));
    assert_eq!(
        symbol(
            &file,
            K::Method,
            "Billing.Services.SubscriptionService.Helper"
        )
        .visibility,
        Some(Visibility::Private)
    );
    assert_eq!(
        imports(&file),
        ["System", "System.Collections.Generic", "System.Math"]
    );
    // The namespace is larger than the target: it is transparent, its
    // members are top-level units and its own lines are leftovers.
    let list = chunked(&file, CSHARP, &options(256));
    assert_covers(CSHARP, &list);
    assert!(list.iter().all(|c| c.kind != ChunkKind::Module));
    assert!(
        list[0].kind == ChunkKind::TopLevel && list[0].text.contains("namespace Billing.Services")
    );
}

#[test]
fn csharp_file_scoped_namespace() {
    let text = "namespace Billing.Core;\n\npublic class A\n{\n    public void M() {}\n}\n";
    let file = parsed("A.cs", text);
    expect_symbols(
        &file,
        &[
            (K::Module, "Billing.Core", 1, 6),
            (K::Class, "Billing.Core.A", 3, 6),
            (K::Method, "Billing.Core.A.M", 5, 5),
        ],
    );
    assert_eq!(
        symbol(&file, K::Module, "Billing.Core").signature,
        "namespace Billing.Core;"
    );
    let skeleton = skeleton_of(&file, text);
    assert_eq!(
        skeleton,
        "namespace Billing.Core;\npublic class A {\n    public void M() { … }\n}\n"
    );
}

const DART: &str = r#"import 'package:flutter/material.dart';
import 'src/api.dart' as api;

/// A subscription client.
class SubscriptionClient extends Base with Mixin {
  final Dio _dio;

  SubscriptionClient(this._dio);

  /// Cancels it.
  Future<void> cancelSubscription(String id) async {
    await _dio.delete('/subscriptions/$id');
  }

  int get count => 1;
}

enum Status { active, cancelled }

mixin Mixin {}

extension StringX on String {
  bool get isBlank => trim().isEmpty;
}

void topLevel() {}

typedef Callback = void Function(int);
"#;

#[test]
fn dart() {
    let file = parsed("lib/client.dart", DART);
    assert_eq!(
        (file.language, file.tier),
        (Language::Dart, Tier::Structural)
    );
    expect_symbols(
        &file,
        &[
            (K::Class, "SubscriptionClient", 4, 16),
            (K::Field, "SubscriptionClient._dio", 6, 6),
            (
                K::Constructor,
                "SubscriptionClient.SubscriptionClient",
                8,
                8,
            ),
            (K::Method, "SubscriptionClient.cancelSubscription", 10, 13),
            (K::Method, "SubscriptionClient.count", 15, 15),
            (K::Enum, "Status", 18, 18),
            (K::Trait, "Mixin", 20, 20),
            (K::Impl, "StringX", 22, 24),
            (K::Method, "StringX.isBlank", 23, 23),
            (K::Function, "topLevel", 26, 26),
            (K::TypeAlias, "Callback", 28, 28),
        ],
    );
    assert_eq!(
        symbol(&file, K::Field, "SubscriptionClient._dio").visibility,
        Some(Visibility::Private)
    );
    assert_eq!(
        symbol(&file, K::Method, "SubscriptionClient.cancelSubscription")
            .doc
            .as_deref(),
        Some("Cancels it.")
    );
    assert_eq!(
        imports(&file),
        ["package:flutter/material.dart", "src/api.dart"]
    );
}

const SWIFT: &str = r#"import Foundation
import UIKit

/// A subscription client.
public final class SubscriptionClient: Base {
    private let session: URLSession

    init(session: URLSession) { self.session = session }

    /// Cancels it.
    public func cancelSubscription(id: String) async throws {
        _ = try await session.data(from: URL(string: "x")!)
    }

    var count: Int { 1 }
}

struct Plan: Codable {
    let id: String
}

protocol Repo {
    func load(id: String) -> Plan?
}

enum Status { case active, cancelled }

extension Plan {
    func describe() -> String { id }
}

func topLevel() {}

typealias PlanID = String
"#;

#[test]
fn swift() {
    let file = parsed("Sources/Billing/Client.swift", SWIFT);
    assert_eq!(file.language, Language::Swift);
    expect_symbols(
        &file,
        &[
            (K::Class, "SubscriptionClient", 4, 16),
            (K::Field, "SubscriptionClient.session", 6, 6),
            (K::Constructor, "SubscriptionClient.init", 8, 8),
            (K::Method, "SubscriptionClient.cancelSubscription", 10, 13),
            (K::Field, "SubscriptionClient.count", 15, 15),
            (K::Struct, "Plan", 18, 20),
            (K::Field, "Plan.id", 19, 19),
            (K::Interface, "Repo", 22, 24),
            (K::Method, "Repo.load", 23, 23),
            (K::Enum, "Status", 26, 26),
            (K::Impl, "Plan", 28, 30),
            (K::Method, "Plan.describe", 29, 29),
            (K::Function, "topLevel", 32, 32),
            (K::TypeAlias, "PlanID", 34, 34),
        ],
    );
    assert_eq!(
        symbol(&file, K::Method, "SubscriptionClient.cancelSubscription").visibility,
        Some(Visibility::Public)
    );
    assert_eq!(
        symbol(&file, K::Field, "SubscriptionClient.session").visibility,
        Some(Visibility::Private)
    );
    assert_eq!(imports(&file), ["Foundation", "UIKit"]);
}

const PHP: &str = r#"<?php

namespace App\Services;

use App\Models\Subscription;
use Illuminate\Support\Facades\DB;
require_once 'helpers.php';

/**
 * Subscription service.
 */
class SubscriptionService extends Base implements Cancellable
{
    private const MAX = 3;
    protected $repo;

    /**
     * Cancels it.
     */
    public function cancelSubscription(string $id): bool
    {
        return $this->repo->delete($id);
    }

    private static function helper() {}
}

interface Cancellable
{
    public function cancelSubscription(string $id): bool;
}

trait Loggable {
    public function log() {}
}

enum Status: string { case Active = 'a'; }

function top_level() {}
"#;

#[test]
fn php() {
    let file = parsed("app/Services/SubscriptionService.php", PHP);
    assert_eq!(file.language, Language::Php);
    expect_symbols(
        &file,
        &[
            (K::Module, "App\\Services", 3, 39),
            (K::Class, "App\\Services.SubscriptionService", 9, 26),
            (K::Constant, "App\\Services.SubscriptionService.MAX", 14, 14),
            (K::Field, "App\\Services.SubscriptionService.$repo", 15, 15),
            (
                K::Method,
                "App\\Services.SubscriptionService.cancelSubscription",
                17,
                23,
            ),
            (
                K::Method,
                "App\\Services.SubscriptionService.helper",
                25,
                25,
            ),
            (K::Interface, "App\\Services.Cancellable", 28, 31),
            (K::Trait, "App\\Services.Loggable", 33, 35),
            (K::Enum, "App\\Services.Status", 37, 37),
            (K::Function, "App\\Services.top_level", 39, 39),
        ],
    );
    assert_eq!(
        symbol(&file, K::Module, "App\\Services").signature,
        "namespace App\\Services;"
    );
    assert_eq!(
        symbol(&file, K::Field, "App\\Services.SubscriptionService.$repo").visibility,
        Some(Visibility::Protected)
    );
    assert_eq!(
        imports(&file),
        [
            "App\\Models\\Subscription",
            "Illuminate\\Support\\Facades\\DB",
            "helpers.php"
        ]
    );
}

const RUBY: &str = r#"require 'json'
require_relative 'models/plan'

# Billing namespace.
module Billing
  # Subscription service.
  class SubscriptionService < Base
    MAX = 3

    # Cancels it.
    def cancel_subscription(id)
      repo.delete(id)
    end

    def self.build
      new
    end

    private

    def helper; end
  end
end

def top_level; end
"#;

#[test]
fn ruby() {
    let file = parsed("lib/billing/subscription_service.rb", RUBY);
    assert_eq!(file.language, Language::Ruby);
    expect_symbols(
        &file,
        &[
            (K::Module, "Billing", 4, 23),
            (K::Class, "Billing.SubscriptionService", 6, 22),
            (K::Constant, "Billing.SubscriptionService.MAX", 8, 8),
            (
                K::Method,
                "Billing.SubscriptionService.cancel_subscription",
                10,
                13,
            ),
            (K::Method, "Billing.SubscriptionService.build", 15, 17),
            (K::Method, "Billing.SubscriptionService.helper", 21, 21),
            (K::Function, "top_level", 25, 25),
        ],
    );
    assert_eq!(
        symbol(&file, K::Module, "Billing").signature,
        "module Billing"
    );
    assert_eq!(
        symbol(&file, K::Class, "Billing.SubscriptionService")
            .doc
            .as_deref(),
        Some("Subscription service.")
    );
    assert_eq!(imports(&file), ["json", "models/plan"]);
    let skeleton = skeleton_of(&file, RUBY);
    assert!(skeleton.starts_with("# Billing namespace.\nmodule Billing\n    # Subscription service.\n    class SubscriptionService < Base\n"), "{skeleton}");
    assert!(skeleton.contains("        def cancel_subscription(id) … end\n"));
    assert!(skeleton.contains("    end\nend\n"));
}

const C: &str = r#"#include <stdio.h>
#include "billing.h"

#define MAX_RETRIES 3

/* A subscription. */
struct subscription {
    int id;
    char *plan;
};

typedef struct subscription subscription_t;

enum status { ACTIVE, CANCELLED };

/** Cancels it. */
int cancel_subscription(struct subscription *s, const char *reason) {
    return 0;
}

static void helper(void) {}

int declared_only(int x);
"#;

#[test]
fn c() {
    let file = parsed("src/billing.c", C);
    assert_eq!((file.language, file.tier), (Language::C, Tier::Structural));
    expect_symbols(
        &file,
        &[
            (K::Macro, "MAX_RETRIES", 4, 4),
            (K::Struct, "subscription", 6, 10),
            (K::Field, "subscription.id", 8, 8),
            (K::Field, "subscription.plan", 9, 9),
            (K::TypeAlias, "subscription_t", 12, 12),
            (K::Enum, "status", 14, 14),
            (K::Function, "cancel_subscription", 16, 19),
            (K::Function, "helper", 21, 21),
            (K::Function, "declared_only", 23, 23),
        ],
    );
    assert_eq!(
        symbol(&file, K::Function, "helper").visibility,
        Some(Visibility::Private)
    );
    assert!(!symbol(&file, K::Function, "declared_only").has_body);
    assert_eq!(imports(&file), ["stdio.h", "billing.h"]);
}

const CPP: &str = r#"#include <vector>
#include "service.hpp"

namespace billing {

/// Subscription service.
class SubscriptionService : public Base {
public:
    explicit SubscriptionService(Repo& repo);
    /// Cancels it.
    bool cancelSubscription(const std::string& id);
private:
    Repo& repo_;
};

bool SubscriptionService::cancelSubscription(const std::string& id) {
    return repo_.remove(id);
}

template <typename T>
T identity(T value) { return value; }

struct Point { int x; int y; };

enum class Status { Active, Cancelled };

}  // namespace billing
"#;

#[test]
fn cpp() {
    let file = parsed("src/service.cpp", CPP);
    assert_eq!(file.language, Language::Cpp);
    expect_symbols(
        &file,
        &[
            (K::Module, "billing", 4, 27),
            (K::Class, "billing.SubscriptionService", 6, 14),
            (
                K::Constructor,
                "billing.SubscriptionService.SubscriptionService",
                9,
                9,
            ),
            (
                K::Method,
                "billing.SubscriptionService.cancelSubscription",
                10,
                11,
            ),
            (K::Field, "billing.SubscriptionService.repo_", 13, 13),
            // Out-of-line definition: the qualifier becomes the receiver.
            (
                K::Method,
                "billing.SubscriptionService.cancelSubscription",
                16,
                18,
            ),
            (K::Function, "billing.identity", 20, 21),
            (K::Struct, "billing.Point", 23, 23),
            (K::Enum, "billing.Status", 25, 25),
        ],
    );
    assert_eq!(
        symbol(&file, K::Function, "billing.identity").signature,
        "template <typename T>\nT identity(T value)"
    );
    assert_eq!(imports(&file), ["vector", "service.hpp"]);
}

const SCALA: &str = r#"package com.example.billing

import scala.concurrent.Future
import com.example.{Plan, Repo}

/** Subscription service. */
class SubscriptionService(repo: Repo) extends Base {
  private val max = 3

  /** Cancels it. */
  def cancelSubscription(id: String): Future[Boolean] = {
    repo.delete(id)
  }
}

trait Cancellable {
  def cancelSubscription(id: String): Future[Boolean]
}

object SubscriptionService {
  def apply(repo: Repo): SubscriptionService = new SubscriptionService(repo)
}

case class Plan(id: String)

type PlanId = String
"#;

#[test]
fn scala() {
    let file = parsed("src/main/scala/SubscriptionService.scala", SCALA);
    assert_eq!(file.language, Language::Scala);
    expect_symbols(
        &file,
        &[
            (K::Class, "SubscriptionService", 6, 14),
            (K::Field, "SubscriptionService.max", 8, 8),
            (K::Method, "SubscriptionService.cancelSubscription", 10, 13),
            (K::Trait, "Cancellable", 16, 18),
            (K::Method, "Cancellable.cancelSubscription", 17, 17),
            (K::Class, "SubscriptionService", 20, 22),
            (K::Method, "SubscriptionService.apply", 21, 21),
            (K::Class, "Plan", 24, 24),
            (K::TypeAlias, "PlanId", 26, 26),
        ],
    );
    assert_eq!(
        symbol(&file, K::Method, "SubscriptionService.apply").signature,
        "def apply(repo: Repo): SubscriptionService"
    );
    assert_eq!(
        imports(&file),
        ["scala.concurrent.Future", "com.example.{Plan, Repo}"]
    );
}

#[test]
fn bash() {
    let text = "#!/usr/bin/env bash\nsource ./lib.sh\n. \"$HOME/.profile.d/x.sh\"\n\n# Deploys the app.\ndeploy() {\n  echo \"deploying\"\n}\n\nfunction cleanup {\n  rm -rf /tmp/x\n}\n";
    let file = parsed("scripts/deploy", text);
    assert_eq!(
        (file.language, file.tier),
        (Language::Bash, Tier::Structural)
    );
    expect_symbols(
        &file,
        &[
            (K::Function, "deploy", 5, 8),
            (K::Function, "cleanup", 10, 12),
        ],
    );
    assert_eq!(
        symbol(&file, K::Function, "deploy").doc.as_deref(),
        Some("Deploys the app.")
    );
    assert_eq!(imports(&file), ["./lib.sh", "$HOME/.profile.d/x.sh"]);
}

#[test]
fn css() {
    let text = "@import url(\"base.css\");\n\n/* Buttons */\n.button, .btn {\n  color: red;\n}\n\n@media (max-width: 600px) {\n  .button { color: blue; }\n}\n";
    let file = parsed("web/app.css", text);
    expect_symbols(
        &file,
        &[
            (K::Rule, ".button, .btn", 3, 6),
            (K::Rule, "@media (max-width: 600px)", 8, 10),
            (K::Rule, "@media (max-width: 600px) .button", 9, 9),
        ],
    );
    assert_eq!(imports(&file), ["base.css"]);
}

#[test]
fn text_only_languages_have_no_symbols() {
    let text = "<html>\n<body>\n<h1>Billing</h1>\n</body>\n</html>\n";
    let file = parsed("web/index.html", text);
    assert_eq!((file.language, file.tier), (Language::Html, Tier::TextOnly));
    assert!(file.symbols.is_empty() && file.imports.is_empty() && file.degraded.is_none());
    assert_eq!(skeleton_of(&file, text), "");
    let list = chunked(&file, text, &ChunkOptions::default());
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].kind, ChunkKind::File);
}
