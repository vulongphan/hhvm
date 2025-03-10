/*
   +----------------------------------------------------------------------+
   | HipHop for PHP                                                       |
   +----------------------------------------------------------------------+
   | Copyright (c) 2010-present Facebook, Inc. (http://www.facebook.com)  |
   +----------------------------------------------------------------------+
   | This source file is subject to version 3.01 of the PHP license,      |
   | that is bundled with this package in the file LICENSE, and is        |
   | available through the world-wide-web at the following url:           |
   | http://www.php.net/license/3_01.txt                                  |
   | If you did not receive a copy of the PHP license and are unable to   |
   | obtain it through the world-wide-web, please send a note to          |
   | license@php.net so we can mail you a copy immediately.               |
   +----------------------------------------------------------------------+
*/

#include "hphp/runtime/vm/unit-parser.h"

#include <cinttypes>
#include <condition_variable>
#include <fstream>
#include <iterator>
#include <memory>
#include <mutex>
#include <signal.h>
#include <sstream>
#include <stdio.h>
#include <string>
#include <sys/types.h>
#include <sys/wait.h>

#include <folly/compression/Zstd.h>
#include <folly/DynamicConverter.h>
#include <folly/json.h>
#include <folly/FileUtil.h>
#include <folly/system/ThreadName.h>

#include "hphp/hack/src/facts/ffi_bridge/rust_facts_ffi_bridge.rs"
#include "hphp/hack/src/hhbc/ffi_bridge/rust_compile_ffi_bridge.rs"
#include "hphp/hack/src/hhbc/compile_ffi.h"
#include "hphp/hack/src/hhbc/compile_ffi_types.h"
#include "hphp/hack/src/hhbc/hhbc_by_ref/hhbc-ast.h"
#include "hphp/hack/src/parser/ffi_bridge/rust_parser_ffi_bridge.rs"
#include "hphp/runtime/base/autoload-map.h"
#include "hphp/runtime/base/autoload-handler.h"
#include "hphp/runtime/base/file-stream-wrapper.h"
#include "hphp/runtime/base/ini-setting.h"
#include "hphp/runtime/base/stream-wrapper-registry.h"
#include "hphp/runtime/vm/native.h"
#include "hphp/runtime/vm/hhvm_decl_provider.h"
#include "hphp/runtime/vm/unit-emitter.h"
#include "hphp/util/atomic-vector.h"
#include "hphp/util/embedded-data.h"
#include "hphp/util/gzip.h"
#include "hphp/util/hackc-log.h"
#include "hphp/util/light-process.h"
#include "hphp/util/logger.h"
#include "hphp/util/match.h"
#include "hphp/util/sha1.h"
#include "hphp/util/struct-log.h"
#include "hphp/util/timer.h"
#include "hphp/zend/zend-strtod.h"

namespace HPHP {

TRACE_SET_MOD(extern_compiler);

UnitEmitterCacheHook g_unit_emitter_cache_hook = nullptr;
static std::string s_misc_config;

namespace {

struct CompileException : Exception {
  template<class... A>
  explicit CompileException(A&&... args)
    : Exception(folly::sformat(std::forward<A>(args)...))
  {}
};

[[noreturn]] void throwErrno(const char* what) {
  throw CompileException("{}: {}", what, folly::errnoStr(errno));
}

CompilerResult assemble_string_handle_errors(const char* code,
                                             const std::string& hhas,
                                             const char* filename,
                                             const SHA1& sha1,
                                             const Native::FuncTable& nativeFuncs,
                                             bool& internal_error,
                                             CompileAbortMode mode) {
  try {
    return assemble_string(hhas.c_str(),
                           hhas.length(),
                           filename,
                           sha1,
                           nativeFuncs,
                           false);  /* swallow errors */
  } catch (const FatalErrorException&) {
    throw;
  } catch (const AssemblerFatal& ex) {
    // Assembler returned an error when building this unit
    if (mode >= CompileAbortMode::VerifyErrors) internal_error = true;
    return ex.what();
  } catch (const AssemblerUnserializationError& ex) {
    // Variable unserializer threw when called from the assembler, treat it
    // as an internal error.
    internal_error = true;
    return ex.what();
  } catch (const AssemblerError& ex) {
    if (mode >= CompileAbortMode::VerifyErrors) internal_error = true;

    if (RuntimeOption::EvalHackCompilerVerboseErrors) {
      auto const msg = folly::sformat(
        "{}\n"
        "========== PHP Source ==========\n"
        "{}\n"
        "========== HackC Result ==========\n"
        "{}\n",
        ex.what(),
        code,
        hhas
      );
      Logger::FError("HackC Generated a bad unit: {}", msg);
      return msg;
    } else {
      return ex.what();
    }
  } catch (const std::exception& ex) {
    internal_error = true;
    return ex.what();
  }
}

////////////////////////////////////////////////////////////////////////////////

struct ConfigBuilder {
  template<typename T>
  ConfigBuilder& addField(folly::StringPiece key, const T& data) {
    if (!m_config.isObject()) {
      m_config = folly::dynamic::object();
    }

    m_config[key] = folly::dynamic::object(
      "global_value", folly::toDynamic(data));

    return *this;
  }

  std::string toString() const {
    return m_config.isNull() ? "" : folly::toJson(m_config);
  }

 private:
  folly::dynamic m_config{nullptr};
};

CompilerResult hackc_compile(
  const char* code,
  const char* filename,
  const SHA1& sha1,
  const Native::FuncTable& nativeFuncs,
  bool forDebuggerEval,
  bool& internal_error,
  const RepoOptions& options,
  CompileAbortMode mode
) {

  std::uint8_t flags = make_env_flags(
    !SystemLib::s_inited,           // is_systemlib
    false,                          // is_evaled
    forDebuggerEval,                // for_debugger_eval
    true,                           // dump_symbol_refs
    false,                          // disable_toplevel_elaboration
    RuntimeOption::EvalEnableDecl   // enable_decl
  );

  std::string aliased_namespaces = options.getAliasedNamespacesConfig();
  NativeEnv const native_env{
    filename,
    aliased_namespaces,
    s_misc_config,
    RuntimeOption::EvalEmitClassPointers,
    RuntimeOption::CheckIntOverflow,
    options.getCompilerFlags(),
    options.getParserFlags(),
    flags
  };

  auto const hhas =
    std::string(hackc_compile_from_text_cpp_ffi(native_env, code));

  return assemble_string_handle_errors(code,
                                       hhas,
                                       filename,
                                       sha1,
                                       nativeFuncs,
                                       internal_error,
                                       mode);
}
////////////////////////////////////////////////////////////////////////////////
}

void compilers_start() {
  // Some configs, like IncludeRoots, can't easily be Config::Bind(ed), so here
  // we create a place to dump miscellaneous config values HackC might want.
  s_misc_config = []() -> std::string {
    if (RuntimeOption::EvalHackCompilerInheritConfig) {
      return ConfigBuilder()
        .addField("hhvm.include_roots", RuntimeOption::IncludeRoots)
        .toString();
    }
    return "";
  }();
}

ParseFactsResult extract_facts(
  const std::string& filename,
  const std::string& code,
  const RepoOptions& options
) {
  auto const get_facts = [&](const std::string& source_text) -> ParseFactsResult {
    try {
      auto facts = hackc_extract_facts_as_json_cpp_ffi(options.getFactsFlags(),
                                                 filename,
                                                 source_text);
      return FactsJSONString { std::string(facts) };
    } catch (const std::exception& e) {
      return FactsJSONString { "" }; // Swallow errors from HackC
    }
  };

  if (!code.empty()) {
    return get_facts(code);
  } else {
    auto w = Stream::getWrapperFromURI(StrNR(filename));
    if (!(w && dynamic_cast<FileStreamWrapper*>(w))) {
      throwErrno("Failed to extract facts: Could not get FileStreamWrapper.");
    }
    const auto f = w->open(StrNR(filename), "r", 0, nullptr);
    if (!f) throwErrno("Failed to extract facts: Could not read source code.");
    auto const str = f->read();
    return get_facts(str.get()->toCppString());
  }
}

FfpResult ffp_parse_file(
  const std::string& contents,
  const RepoOptions& options
) {
  auto const env = options.getParserEnvironment();
  auto const parse_tree = hackc_parse_positioned_full_trivia_cpp_ffi(contents, env);
  return FfpJSONString {  std::string(parse_tree) };
}

std::unique_ptr<UnitCompiler>
UnitCompiler::create(const char* code,
                     int codeLen,
                     const char* filename,
                     const SHA1& sha1,
                     const Native::FuncTable& nativeFuncs,
                     bool forDebuggerEval,
                     const RepoOptions& options) {
  auto make = [code, codeLen, filename, sha1, forDebuggerEval,
               &nativeFuncs, &options] {
    return std::make_unique<HackcUnitCompiler>(
      code,
      codeLen,
      filename,
      sha1,
      nativeFuncs,
      forDebuggerEval,
      options
    );
  };

  if (g_unit_emitter_cache_hook && !forDebuggerEval) {
    return std::make_unique<CacheUnitCompiler>(
      code,
      codeLen,
      filename,
      sha1,
      nativeFuncs,
      false,
      options,
      std::move(make)
    );
  } else {
    return make();
  }
}

std::unique_ptr<UnitEmitter> HackcUnitCompiler::compile(
    bool& cacheHit,
    CompileAbortMode mode) {
  auto ice = false;
  cacheHit = false;
  auto res = hackc_compile(m_code,
                           m_filename,
                           m_sha1,
                           m_nativeFuncs,
                           m_forDebuggerEval,
                           ice,
                           m_options,
                           mode);
  auto unitEmitter = match<std::unique_ptr<UnitEmitter>>(
    res,
    [&] (std::unique_ptr<UnitEmitter>& ue) {
      ue->finish();
      return std::move(ue);
    },
    [&] (std::string& err) {
      switch (mode) {
      case CompileAbortMode::Never:
        break;
      case CompileAbortMode::AllErrorsNull: {
        auto ue = std::unique_ptr<UnitEmitter>{};
        ue->finish();
        return ue;
      }
      case CompileAbortMode::OnlyICE:
      case CompileAbortMode::VerifyErrors:
      case CompileAbortMode::AllErrors:
        // run_compiler will promote errors to ICE as appropriate based on mode
        if (ice) {
          fprintf(
            stderr,
            "Encountered an internal error while processing HHAS for %s, "
            "bailing because Eval.AbortBuildOnCompilerError is set\n\n%s",
            m_filename, err.data()
          );
          _Exit(1);
        }
      }
      return createFatalUnit(
        makeStaticString(m_filename),
        m_sha1,
        FatalOp::Runtime,
        err
      );
    }
  );

  if (unitEmitter) unitEmitter->m_ICE = ice;
  return unitEmitter;
}

std::unique_ptr<UnitEmitter>
CacheUnitCompiler::compile(bool& cacheHit, CompileAbortMode mode) {
  assertx(g_unit_emitter_cache_hook);
  cacheHit = true;
  return g_unit_emitter_cache_hook(
    m_filename,
    m_sha1,
    m_codeLen,
    [&] (bool wantsICE) {
      if (!m_fallback) m_fallback = m_makeFallback();
      assertx(m_fallback);
      return m_fallback->compile(
        cacheHit,
        wantsICE ? mode : CompileAbortMode::AllErrorsNull
      );
    },
    m_nativeFuncs
  );
}

////////////////////////////////////////////////////////////////////////////////
}
