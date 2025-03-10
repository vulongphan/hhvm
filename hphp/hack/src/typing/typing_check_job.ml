(*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the "hack" directory of this source tree.
 *
 *)

open Hh_prelude

(*
####

The type checking service receives a list of files and their symbols as input and
distributes the work to worker processes.

In this file you will only find functions to process a single file at a time
*)

(*****************************************************************************)
(* The job that will be run on the workers *)
(*****************************************************************************)

let handle_exn_as_error : type res. Pos.t -> (unit -> res option) -> res option
    =
 fun pos f ->
  try f () with
  | (WorkerCancel.Worker_should_exit | Deferred_decl.Defer _) as e ->
    (* Cancellation requests must be re-raised *)
    raise e
  | e ->
    let typechecking_is_deferring = Deferred_decl.is_deferring () in
    Errors.exception_occurred ~typechecking_is_deferring pos (Exception.wrap e);
    None

let type_fun (ctx : Provider_context.t) (fn : Relative_path.t) (x : string) :
    (Tast.def * Typing_inference_env.t_global_with_pos) option =
  match Ast_provider.find_fun_in_file ~full:true ctx fn x with
  | Some fd ->
    let f = fd.Aast.fd_fun in
    handle_exn_as_error f.Aast.f_span (fun () ->
        let fun_ = Naming.fun_def ctx fd in
        Nast_check.def ctx (Aast.Fun fun_);
        let def_opt =
          Typing_toplevel.fun_def ctx fun_
          |> Option.map ~f:(fun (f, global_tvenv) -> (Aast.Fun f, global_tvenv))
        in
        Option.iter def_opt ~f:(fun (f, _) -> Tast_check.def ctx f);
        def_opt)
  | None -> None

let type_class (ctx : Provider_context.t) (fn : Relative_path.t) (x : string) :
    (Tast.def * Typing_inference_env.t_global_with_pos list) option =
  match Ast_provider.find_class_in_file ~full:true ctx fn x with
  | Some cls ->
    handle_exn_as_error cls.Aast.c_span (fun () ->
        let class_ = Naming.class_ ctx cls in
        Nast_check.def ctx (Aast.Class class_);
        let def_opt =
          Typing_toplevel.class_def ctx class_
          |> Option.map ~f:(fun (c, global_tvenv) ->
                 (Aast.Class c, global_tvenv))
        in
        Option.iter def_opt ~f:(fun (f, _) -> Tast_check.def ctx f);
        def_opt)
  | None -> None

let type_record_def
    (ctx : Provider_context.t) (fn : Relative_path.t) (x : string) :
    Tast.def option =
  match Ast_provider.find_record_def_in_file ~full:true ctx fn x with
  | Some rd ->
    handle_exn_as_error rd.Aast.rd_span (fun () ->
        let rd = Naming.record_def ctx rd in
        Nast_check.def ctx (Aast.RecordDef rd);

        let def = Aast.RecordDef (Typing_toplevel.record_def_def ctx rd) in
        Tast_check.def ctx def;
        Some def)
  | None -> None

let check_typedef (ctx : Provider_context.t) (fn : Relative_path.t) (x : string)
    : Tast.def option =
  match Ast_provider.find_typedef_in_file ~full:true ctx fn x with
  | Some t ->
    handle_exn_as_error Pos.none (fun () ->
        let typedef = Naming.typedef ctx t in
        Nast_check.def ctx (Aast.Typedef typedef);
        let ret = Typing.typedef_def ctx typedef in
        let def = Aast.Typedef ret in
        Tast_check.def ctx def;
        Some def)
  | None -> None

let check_const (ctx : Provider_context.t) (fn : Relative_path.t) (x : string) :
    Tast.def option =
  match Ast_provider.find_gconst_in_file ~full:true ctx fn x with
  | None -> None
  | Some cst ->
    handle_exn_as_error cst.Aast.cst_span (fun () ->
        let cst = Naming.global_const ctx cst in
        Nast_check.def ctx (Aast.Constant cst);
        let def = Aast.Constant (Typing_toplevel.gconst_def ctx cst) in
        Tast_check.def ctx def;
        Some def)
