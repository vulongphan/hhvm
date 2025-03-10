(*
 * Copyright (c) 2018, Facebook, Inc.
 * All rights reserved.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the "hack" directory of this source tree.
 *
 *)
open Hh_prelude
open Aast
module Env = Tast_env
module Cls = Decl_provider.Class
module SN = Naming_special_names
module MakeType = Typing_make_type
module Reason = Typing_reason

type rty =
  | Readonly
  | Mut [@deriving show]

let readonly_kind_to_rty = function
  | Some Ast_defs.Readonly -> Readonly
  | _ -> Mut

let rty_to_str = function
  | Readonly -> "readonly"
  | Mut -> "mutable"

let pp_rty fmt rty = Format.fprintf fmt "%s" (rty_to_str rty)

(* Returns true if rty_sub is a subtype of rty_sup.
TODO: Later, we'll have to consider the regular type as well, for example
we could allow readonly int as equivalent to an int for devX purposes *)
let subtype_rty rty_sub rty_sup =
  match (rty_sub, rty_sup) with
  | (Readonly, Mut) -> false
  | _ -> true

let param_to_rty param =
  if Typing_defs.get_fp_readonly param then
    Readonly
  else
    Mut

let rec grab_class_elts_from_ty ~static ?(seen = SSet.empty) env ty prop_id =
  let open Typing_defs in
  (* Given a list of types, find recurse on the first type that
     has the property and return the result *)
  let find_first_in_list ~seen tyl =
    List.find_map
      ~f:(fun ty ->
        match grab_class_elts_from_ty ~static ~seen env ty prop_id with
        | [] -> None
        | tyl -> Some tyl)
      tyl
  in
  match get_node ty with
  | Tclass (id, _exact, _args) ->
    let provider_ctx = Tast_env.get_ctx env in
    let class_decl = Decl_provider.get_class provider_ctx (snd id) in
    (match class_decl with
    | Some class_decl ->
      let prop =
        if static then
          Cls.get_sprop class_decl (snd prop_id)
        else
          Cls.get_prop class_decl (snd prop_id)
      in
      Option.to_list prop
    | None -> [])
  (* Accessing a property off of an intersection type
     should involve exactly one kind of readonlyness, since for
     the intersection type to exist, the property must be related
     by some subtyping relationship anyways, and property readonlyness
     is invariant. Thus we just grab the first one from the list where the prop exists. *)
  | Tintersection [] -> []
  | Tintersection tyl ->
    find_first_in_list ~seen tyl |> Option.value ~default:[]
  (* A union type is more interesting, where we must return all possible cases
     and be conservative in our use case. *)
  | Tunion tyl ->
    List.concat_map
      ~f:(fun ty -> grab_class_elts_from_ty ~static ~seen env ty prop_id)
      tyl
  (* Generic types can be treated similarly to an intersection type
     where we find the first prop that works from the upper bounds *)
  | Tgeneric (name, tyargs) ->
    (* Avoid circular generics with a set *)
    if SSet.mem name seen then
      []
    else
      let new_seen = SSet.add name seen in
      let upper_bounds = Tast_env.get_upper_bounds env name tyargs in
      find_first_in_list ~seen:new_seen (Typing_set.elements upper_bounds)
      |> Option.value ~default:[]
  | Tdependent (_, ty) ->
    (* Dependent types have an upper bound that's a class or generic *)
    grab_class_elts_from_ty ~static ~seen env ty prop_id
  (* TODO: Handle more complex types *)
  | _ -> []

(* Return a list of possible static prop elts given a class_get expression *)
let get_static_prop_elts env class_id get =
  let (ty, _, _) = class_id in
  match get with
  | CGstring prop_id -> grab_class_elts_from_ty ~static:true env ty prop_id
  (* An expression is dynamic, so there's no way to tell the type generally *)
  | CGexpr _ -> []

(* Return a list of possible prop elts given an obj get expression *)
let get_prop_elts env obj get =
  let ty = Tast.get_type obj in
  match get with
  | (_, _, Id prop_id) -> grab_class_elts_from_ty ~static:false env ty prop_id
  (* TODO: Handle more complex  cases *)
  | _ -> []

let rec ty_expr env ((_, _, expr_) : Tast.expr) : rty =
  match expr_ with
  | ReadonlyExpr _ -> Readonly
  (* Obj_get, array_get, and class_get are here for better error messages when used as lval *)
  | Obj_get (e1, e2, _, _) ->
    (match ty_expr env e1 with
    | Readonly -> Readonly
    | Mut ->
      (* In the mut case, we need to check if the property is marked readonly *)
      let prop_elts = get_prop_elts env e1 e2 in
      let readonly_prop =
        List.find ~f:Typing_defs.get_ce_readonly_prop prop_elts
      in
      Option.value_map readonly_prop ~default:Mut ~f:(fun _ -> Readonly))
  | Class_get (class_id, expr, _is_prop_call) ->
    (* If any of the static props could be readonly, treat the expression as readonly *)
    let class_elts = get_static_prop_elts env class_id expr in
    (* Note that the empty list case (when the prop doesn't exist) returns Mut *)
    if List.exists class_elts ~f:Typing_defs.get_ce_readonly_prop then
      Readonly
    else
      Mut
  | Array_get (array, _) -> ty_expr env array
  | _ -> Mut

let is_value_collection_ty env ty =
  let mixed = MakeType.mixed Reason.none in
  let env = Tast_env.tast_env_as_typing_env env in
  let hackarray = MakeType.any_array Reason.none mixed mixed in
  (* Subtype against an empty open shape (shape(...)) *)
  let shape =
    MakeType.shape
      Reason.none
      Typing_defs.Open_shape
      Typing_defs.TShapeMap.empty
  in
  Typing_utils.is_sub_type env ty hackarray
  || Typing_utils.is_sub_type env ty shape

(* Check if type is safe to convert from readonly to mut
    TODO(readonly): Update to include more complex types. *)
let rec is_safe_mut_ty env (seen : SSet.t) ty =
  let open Typing_defs_core in
  match get_node ty with
  | Tshape (Open_shape, _) -> false
  | Tshape (Closed_shape, fields) ->
    TShapeMap.for_all (fun _k v -> is_safe_mut_ty env seen v.sft_ty) fields
  (* If it's a Tclass it's an array type by is_value_collection *)
  | Tintersection tyl -> List.exists tyl ~f:(fun l -> is_safe_mut_ty env seen l)
  | Tunion tyl -> List.exists tyl ~f:(fun l -> is_safe_mut_ty env seen l)
  | Tdependent (_, upper) ->
    (* check upper bounds *)
    is_safe_mut_ty env seen upper
  | Tclass (_, _, tyl) when is_value_collection_ty env ty ->
    List.for_all tyl ~f:(fun l -> is_safe_mut_ty env seen l)
  | Tgeneric (name, tyargs) ->
    (* Avoid circular generics with a set *)
    if SSet.mem name seen then
      false
    else
      let new_seen = SSet.add name seen in
      let upper_bounds = Tast_env.get_upper_bounds env name tyargs in
      Typing_set.exists (fun l -> is_safe_mut_ty env new_seen l) upper_bounds
  | _ ->
    (* Otherwise, check if it's primitive *)
    let env = Tast_env.tast_env_as_typing_env env in
    let primitive_types =
      [
        MakeType.bool Reason.none;
        MakeType.int Reason.none;
        MakeType.arraykey Reason.none;
        MakeType.string Reason.none;
        MakeType.float Reason.none;
        MakeType.null Reason.none;
        MakeType.num Reason.none;
        (* Keysets only contain arraykeys so if they're readonly its safe to remove *)
        MakeType.keyset Reason.none (MakeType.arraykey Reason.none);
      ]
    in
    (* Make a union type to subtype with the ty *)
    let union = MakeType.union Reason.none primitive_types in
    Typing_utils.is_sub_type env ty union

(* Check that function calls which return readonly are wrapped in readonly *)
let check_readonly_return_call pos caller_ty is_readonly =
  let open Typing_defs in
  match get_node caller_ty with
  | Tfun fty when get_ft_returns_readonly fty ->
    if not is_readonly then
      Errors.explicit_readonly_cast
        "function call"
        pos
        (Typing_defs.get_pos caller_ty)
  | _ -> ()

let check_readonly_property env obj get obj_ro =
  let open Typing_defs in
  let prop_elts = get_prop_elts env obj get in
  (* If there's any property in the list of possible properties that could be readonly,
      it must be explicitly cast to readonly *)
  let readonly_prop = List.find ~f:get_ce_readonly_prop prop_elts in
  match (readonly_prop, obj_ro) with
  | (Some elt, Mut) ->
    Errors.explicit_readonly_cast
      "property"
      (Tast.get_position get)
      (Lazy.force elt.ce_pos)
  | _ -> ()

let check_static_readonly_property pos env (class_ : Tast.class_id) get obj_ro =
  let prop_elts = get_static_prop_elts env class_ get in
  (* If there's any property in the list of possible properties that could be readonly,
      it must be explicitly cast to readonly *)
  let readonly_prop = List.find ~f:Typing_defs.get_ce_readonly_prop prop_elts in
  match (readonly_prop, obj_ro) with
  | (Some elt, Mut) when Typing_defs.get_ce_readonly_prop elt ->
    Errors.explicit_readonly_cast
      "static property"
      pos
      (Lazy.force elt.Typing_defs.ce_pos)
  | _ -> ()

let is_method_caller (caller : Tast.expr) =
  match caller with
  | (_, _, ReadonlyExpr (_, _, Obj_get (_, _, _, (* is_prop_call *) false)))
  | (_, _, Obj_get (_, _, _, (* is_prop_call *) false)) ->
    true
  | _ -> false

let rec assign env lval rval =
  (* Check that we're assigning a readonly value to a readonly property *)
  let check_ro_prop_assignment prop_elts =
    let mutable_prop =
      List.find ~f:(fun r -> not (Typing_defs.get_ce_readonly_prop r)) prop_elts
    in
    match mutable_prop with
    | Some elt when not (Typing_defs.get_ce_readonly_prop elt) ->
      Errors.readonly_mismatch
        "Invalid property assignment"
        (Tast.get_position lval)
        ~reason_sub:
          [
            ( Tast.get_position rval |> Pos_or_decl.of_raw_pos,
              "This expression is readonly" );
          ]
        ~reason_super:
          [
            ( Lazy.force elt.Typing_defs.ce_pos,
              "But it's being assigned to a mutable property" );
          ]
    | _ -> ()
  in
  match lval with
  | (_, _, Array_get (array, _)) ->
    begin
      match (ty_expr env array, ty_expr env rval) with
      | (Readonly, _) when is_value_collection_ty env (Tast.get_type array) ->
        (* In the case of (expr)[0] = rvalue, where expr is a value collection like vec,
           we need to check assignment recursively because ($x->prop)[0] is only valid if $x is mutable and prop is readonly. *)
        (match array with
        | (_, _, Array_get _)
        | (_, _, Obj_get _) ->
          assign env array rval
        | _ -> ())
      | (Mut, Readonly) ->
        Errors.readonly_mismatch
          "Invalid collection modification"
          (Tast.get_position lval)
          ~reason_sub:
            [
              ( Tast.get_position rval |> Pos_or_decl.of_raw_pos,
                "This expression is readonly" );
            ]
          ~reason_super:
            [
              ( Tast.get_position array |> Pos_or_decl.of_raw_pos,
                "But this value is mutable" );
            ]
      | (Readonly, _) -> Errors.readonly_modified (Tast.get_position array)
      | (Mut, Mut) -> ()
    end
  | (_, _, Class_get (id, expr, _)) ->
    (match ty_expr env rval with
    | Readonly ->
      let prop_elts = get_static_prop_elts env id expr in
      check_ro_prop_assignment prop_elts
    | _ -> ())
  | (_, _, Obj_get (obj, get, _, _)) ->
    (* Here to check for nested property accesses that are accessing readonly values *)
    begin
      match ty_expr env obj with
      | Readonly -> Errors.readonly_modified (Tast.get_position obj)
      | Mut -> ()
    end;
    (match ty_expr env rval with
    | Readonly ->
      let prop_elts = get_prop_elts env obj get in
      (* If there's a mutable prop, then there's a chance we're assigning to one *)
      check_ro_prop_assignment prop_elts
    | _ -> ())
  (* TODO: make this exhaustive *)
  | _ -> ()

(* Method call invocation *)
let method_call caller =
  let open Typing_defs in
  match caller with
  (* Readonly call checks *)
  | (ty, _, ReadonlyExpr (_, _, Obj_get (e1, _, _, false))) ->
    (match get_node ty with
    | Tfun fty when not (get_ft_readonly_this fty) ->
      Errors.readonly_method_call (Tast.get_position e1) (get_pos ty)
    | _ -> ())
  | _ -> ()

let check_special_function env caller args =
  match (caller, args) with
  | ((_, _, Id (pos, x)), [(_, arg)])
    when String.equal (Utils.strip_ns x) (Utils.strip_ns SN.Readonly.as_mut) ->
    let arg_ty = Tast.get_type arg in
    if not (is_safe_mut_ty env SSet.empty arg_ty) then
      Errors.readonly_invalid_as_mut pos
    else
      ()
  | _ -> ()

(* Checks related to calling a function or method
   is_readonly is true when the call is allowed to return readonly
   TODO: handle inout
*)
let call
    ~is_readonly
    ~method_call
    (env : Tast_env.t)
    (pos : Pos.t)
    (caller_ty : Tast.ty)
    (caller_rty : rty)
    (args : (Ast_defs.param_kind * Tast.expr) list)
    (unpacked_arg : Tast.expr option) =
  let open Typing_defs in
  let (env, caller_ty) = Tast_env.expand_type env caller_ty in
  let check_readonly_closure caller_ty caller_rty =
    match (get_node caller_ty, caller_rty) with
    | (Tfun fty, Readonly)
      when (not (get_ft_readonly_this fty)) && not method_call ->
      (* Get the position of why this function is its current type (usually a typehint) *)
      let reason = get_reason caller_ty in
      let f_pos = Reason.to_pos (get_reason caller_ty) in
      let suggestion =
        match reason with
        (* If we got this function from a typehint, we suggest marking the function (readonly function) *)
        | Typing_reason.Rhint _ ->
          let new_flags =
            Typing_defs_flags.(set_bit ft_flags_readonly_this true fty.ft_flags)
          in
          let readonly_fty = Tfun { fty with ft_flags = new_flags } in
          let suggested_fty = mk (reason, readonly_fty) in
          let suggested_fty_str = Tast_env.print_ty env suggested_fty in
          "annotate this typehint as a " ^ suggested_fty_str
        (* Otherwise, it's likely from a Rwitness, but we suggest declaring it as readonly *)
        | _ -> "declaring this as a `readonly` function"
      in
      Errors.readonly_closure_call pos f_pos suggestion
    | _ -> ()
  in
  (* Checks a single arg against a parameter *)
  let check_arg param (_, arg) =
    let param_rty = param_to_rty param in
    let arg_rty = ty_expr env arg in
    if not (subtype_rty arg_rty param_rty) then
      Errors.readonly_mismatch
        "Invalid argument"
        (Tast.get_position arg)
        ~reason_sub:
          [
            ( Tast.get_position arg |> Pos_or_decl.of_raw_pos,
              "This expression is " ^ rty_to_str arg_rty );
          ]
        ~reason_super:
          [
            ( param.fp_pos,
              "It is incompatible with this parameter, which is "
              ^ rty_to_str param_rty );
          ]
  in

  (* Check that readonly arguments match their parameters *)
  let check_args caller_ty args unpacked_arg =
    match get_node caller_ty with
    | Tfun fty ->
      let unpacked_rty =
        unpacked_arg
        |> Option.map ~f:(fun e -> (Ast_defs.Pnormal, e))
        |> Option.to_list
      in
      let args = args @ unpacked_rty in
      (* If the args are unequal length, we errored elsewhere so this does not care *)
      let _ = List.iter2 fty.ft_params args ~f:check_arg in
      ()
    | _ -> ()
  in
  check_readonly_closure caller_ty caller_rty;
  check_readonly_return_call pos caller_ty is_readonly;
  check_args caller_ty args unpacked_arg

let check =
  object (self)
    inherit Tast_visitor.iter as super

    method! on_expr env e =
      match e with
      | (_, _, Binop (Ast_defs.Eq _, lval, rval)) ->
        assign env lval rval;
        self#on_expr env rval
      | (_, _, ReadonlyExpr (_, _, Call (caller, targs, args, unpacked_arg))) ->
        call
          ~is_readonly:true
          ~method_call:(is_method_caller caller)
          env
          (Tast.get_position caller)
          (Tast.get_type caller)
          (ty_expr env caller)
          args
          unpacked_arg;
        check_special_function env caller args;
        method_call caller;
        (* Skip the recursive step into ReadonlyExpr to avoid erroring *)
        self#on_Call env caller targs args unpacked_arg
      (* Non readonly calls *)
      | (_, _, Call (caller, _, args, unpacked_arg)) ->
        call
          env
          ~is_readonly:false
          ~method_call:(is_method_caller caller)
          (Tast.get_position caller)
          (Tast.get_type caller)
          (ty_expr env caller)
          args
          unpacked_arg;
        check_special_function env caller args;
        method_call caller;
        super#on_expr env e
      | (_, _, ReadonlyExpr (_, _, Obj_get (obj, get, nullable, is_prop_call)))
        ->
        (* Skip the recursive step into ReadonlyExpr to avoid erroring *)
        self#on_Obj_get env obj get nullable is_prop_call
      | (_, _, ReadonlyExpr (_, _, Class_get (class_, get, x))) ->
        (* Skip the recursive step into ReadonlyExpr to avoid erroring *)
        self#on_Class_get env class_ get x
      | (_, _, Obj_get (obj, get, _nullable, _is_prop_call)) ->
        check_readonly_property env obj get Mut;
        super#on_expr env e
      | (_, pos, Class_get (class_, get, _is_prop_call)) ->
        check_static_readonly_property pos env class_ get Mut;
        super#on_expr env e
      | (_, pos, New (_, _, args, unpacked_arg, constructor_fty)) ->
        (* Constructors never return readonly, so that specific check is irrelevant *)
        call
          ~is_readonly:false
          ~method_call:false
          env
          pos
          constructor_fty
          Mut
          (List.map ~f:(fun e -> (Ast_defs.Pnormal, e)) args)
          unpacked_arg
      | (_, _, This)
      | (_, _, ValCollection (_, _, _))
      | (_, _, KeyValCollection (_, _, _))
      | (_, _, Lvar _)
      | (_, _, Clone _)
      | (_, _, Array_get (_, _))
      | (_, _, Yield _)
      | (_, _, Await _)
      | (_, _, Tuple _)
      | (_, _, List _)
      | (_, _, Cast (_, _))
      | (_, _, Unop (_, _))
      | (_, _, Pipe (_, _, _))
      | (_, _, Eif (_, _, _))
      | (_, _, Is (_, _))
      | (_, _, As (_, _, _))
      | (_, _, Upcast (_, _))
      | (_, _, Import (_, _))
      | (_, _, Lplaceholder _)
      | (_, _, Pair (_, _, _))
      | (_, _, ReadonlyExpr _)
      | (_, _, Binop _)
      | (_, _, ExpressionTree _)
      | (_, _, Xml _)
      | (_, _, Efun _)
      (* Neither this nor any of the *_id expressions call the function *)
      | (_, _, Method_caller (_, _))
      | (_, _, Smethod_id (_, _))
      | (_, _, Fun_id _)
      | (_, _, Method_id _)
      | (_, _, FunctionPointer _)
      | (_, _, Lfun _)
      | (_, _, Record _)
      | (_, _, Null)
      | (_, _, True)
      | (_, _, False)
      | (_, _, Omitted)
      | (_, _, Id _)
      | (_, _, Shape _)
      | (_, _, EnumClassLabel _)
      | (_, _, ET_Splice _)
      | (_, _, Darray _)
      | (_, _, Varray _)
      | (_, _, Int _)
      | (_, _, Dollardollar _)
      | (_, _, String _)
      | (_, _, String2 _)
      | (_, _, Collection (_, _, _))
      | (_, _, Class_const _)
      | (_, _, Float _)
      | (_, _, PrefixedString _)
      | (_, _, Hole _) ->
        super#on_expr env e
  end

let handler =
  object
    inherit Tast_visitor.handler_base

    method! at_method_ env m =
      let tcopt = Tast_env.get_tcopt env in
      if TypecheckerOptions.readonly tcopt then
        check#on_method_ env m
      else
        ()

    method! at_fun_def env f =
      let tcopt = Tast_env.get_tcopt env in
      if TypecheckerOptions.readonly tcopt then
        check#on_fun_def env f
      else
        ()

    (*
        The following error checks are ones that need to run even if
        readonly analysis is not enabled by the file attribute.

        TODO(readonly): When the user has not enabled readonly
        and theres a readonly keyword, this will incorrectly error
        an extra time on top of the parsing error. Fixing this
        extra error at this stage will require a bunch of added complexity
        and perf cost, and since this will only occur while the
        feature is unstable, we allow the extra error it for now.
      *)
    method! at_Call env caller _tal _el _unpacked_element =
      let tcopt = Tast_env.get_tcopt env in
      (* this check is already handled by the readonly analysis,
         which handles cases when there's a readonly keyword *)
      if TypecheckerOptions.readonly tcopt then
        ()
      else
        let caller_pos = Tast.get_position caller in
        let caller_ty = Tast.get_type caller in
        check_readonly_return_call caller_pos caller_ty false

    method! at_expr env e =
      let tcopt = Tast_env.get_tcopt env in
      (* this check is already handled by the readonly analysis,
         which handles cases when there's a readonly keyword *)
      let check =
        if TypecheckerOptions.readonly tcopt then
          fun _e ->
        ()
        else
          fun e ->
        let val_kind = Tast_env.get_val_kind env in
        match (e, val_kind) with
        | ((_, _, Binop (Ast_defs.Eq _, lval, rval)), _) ->
          (* Check property assignments to make sure they're safe *)
          assign env lval rval
        (* Assume obj is mutable here since you can't have a readonly thing
           without readonly keyword/analysis *)
        (* Only check this for rvalues, not lvalues *)
        | ((_, _, Obj_get (obj, get, _, _)), Typing_defs.Other) ->
          check_readonly_property env obj get Mut
        | ((_, pos, Class_get (class_id, get, _)), Typing_defs.Other) ->
          check_static_readonly_property pos env class_id get Mut
        | _ -> ()
      in
      check e
  end
