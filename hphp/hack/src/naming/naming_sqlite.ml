(*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the "hack" directory of this source tree.
 *
 *)

open Hh_prelude
open Sqlite_utils

type db_path = Db_path of string [@@deriving eq, show]

type insertion_error = {
  canon_hash: Int64.t;
  hash: Int64.t;
  name_kind: Naming_types.name_kind;
  name: string;
  origin_exception: Exception.t;
      [@printer (fun fmt e -> fprintf fmt "%s" (Exception.get_ctor_string e))]
}
[@@deriving show]

type save_result = {
  files_added: int;
  symbols_added: int;
  errors: insertion_error list;
}
[@@deriving show]

let empty_save_result = { files_added = 0; symbols_added = 0; errors = [] }

let insert_safe ~name ~name_kind ~hash ~canon_hash f :
    (unit, insertion_error) result =
  try Ok (f ()) with
  | e ->
    let origin_exception = Exception.wrap e in
    Error { canon_hash; hash; name_kind; name; origin_exception }

type 'a forward_naming_table_delta =
  | Modified of 'a
  | Deleted
[@@deriving show]

type file_deltas = FileInfo.t forward_naming_table_delta Relative_path.Map.t

type blob_format = FileInfo.saved forward_naming_table_delta Relative_path.Map.t

let pp_file_deltas =
  Relative_path.Map.make_pp
    Relative_path.pp
    (pp_forward_naming_table_delta FileInfo.pp)

type local_changes = {
  file_deltas: file_deltas;
  base_content_version: string;
}
[@@deriving show]

let _ = show_forward_naming_table_delta

let _ = show_local_changes

module StatementCache = struct
  type t = {
    db: Sqlite3.db;
    statements: (string, Sqlite3.stmt) Hashtbl.t;
  }

  let make ~db = { db; statements = Hashtbl.Poly.create () }

  (** Prepared statements must be finalized before we can close the database
  connection, or else an exception is thrown. Call this function before
  attempting `Sqlite3.close_db`. *)
  let close t =
    Hashtbl.iter t.statements ~f:(fun stmt ->
        Sqlite3.finalize stmt |> check_rc t.db);
    Hashtbl.clear t.statements

  let make_stmt t query =
    let stmt =
      Hashtbl.find_or_add t.statements query ~default:(fun () ->
          Sqlite3.prepare t.db query)
    in
    (* Clear any previous bindings for prepared statement parameters. *)
    Sqlite3.reset stmt |> check_rc t.db;
    stmt
end

let make_relative_path ~prefix_int ~suffix =
  let prefix =
    let open Option in
    value_exn (Int64.to_int prefix_int >>= Relative_path.prefix_of_enum)
  in
  let full_suffix =
    Filename.concat (Relative_path.path_of_prefix prefix) suffix
  in
  Relative_path.create prefix full_suffix

let to_canon_name_key = Caml.String.lowercase_ascii

let fold_sqlite stmt ~f ~init =
  let rec helper acc =
    match Sqlite3.step stmt with
    | Sqlite3.Rc.DONE -> acc
    | Sqlite3.Rc.ROW -> helper (f stmt acc)
    | rc ->
      failwith
        (Printf.sprintf "SQLite operation failed: %s" (Sqlite3.Rc.to_string rc))
  in
  helper init

(* It's kind of weird to just dump this into the SQLite database, but removing
 * entries one at a time during a normal incremental update leads to too many
 * table scans, and I don't want to put this in a separate file because then
 * the naming table and dep table in a given SQLite database might not agree
 * with each other, and that just feels weird. *)
module LocalChanges = struct
  let table_name = "NAMING_LOCAL_CHANGES"

  let create_table_sqlite =
    Printf.sprintf
      "
          CREATE TABLE IF NOT EXISTS %s(
            ID INTEGER PRIMARY KEY,
            LOCAL_CHANGES BLOB NOT NULL,
            BASE_CONTENT_VERSION TEXT
          );
        "
      table_name

  let insert_sqlite =
    Printf.sprintf
      "
          INSERT INTO %s (ID, LOCAL_CHANGES, BASE_CONTENT_VERSION) VALUES (0, ?, ?);
        "
      table_name

  let update_sqlite =
    Printf.sprintf
      "
          UPDATE %s SET LOCAL_CHANGES = ? WHERE ID = 0;
        "
      table_name

  let get_sqlite =
    Printf.sprintf
      "
          SELECT LOCAL_CHANGES, BASE_CONTENT_VERSION FROM %s;
        "
      table_name

  let prime db stmt_cache base_content_version =
    let insert_stmt = StatementCache.make_stmt stmt_cache insert_sqlite in
    let (blob : blob_format) = Relative_path.Map.empty in
    let empty = Marshal.to_string blob [Marshal.No_sharing] in
    Sqlite3.bind insert_stmt 1 (Sqlite3.Data.BLOB empty) |> check_rc db;
    Sqlite3.bind insert_stmt 2 (Sqlite3.Data.TEXT base_content_version)
    |> check_rc db;
    Sqlite3.step insert_stmt |> check_rc db

  let update db stmt_cache (local_changes : local_changes) =
    if Relative_path.Map.cardinal local_changes.file_deltas > 0 then
      HackEventLogger.naming_sqlite_local_changes_nonempty "update";
    let (local_changes_saved : blob_format) =
      Relative_path.Map.map local_changes.file_deltas ~f:(fun delta ->
          match delta with
          | Modified fi -> Modified (FileInfo.to_saved fi)
          | Deleted -> Deleted)
    in
    let local_changes_blob =
      Marshal.to_string local_changes_saved [Marshal.No_sharing]
    in
    let update_stmt = StatementCache.make_stmt stmt_cache update_sqlite in
    Sqlite3.bind update_stmt 1 (Sqlite3.Data.BLOB local_changes_blob)
    |> check_rc db;
    Sqlite3.step update_stmt |> check_rc db

  let get stmt_cache =
    let get_stmt = StatementCache.make_stmt stmt_cache get_sqlite in
    match Sqlite3.step get_stmt with
    (* We don't include Sqlite3.Rc.DONE in this match because we always expect
     * exactly one row. *)
    | Sqlite3.Rc.ROW ->
      let local_changes_blob = column_blob get_stmt 0 in
      let (local_changes_saved : blob_format) =
        Marshal.from_string local_changes_blob 0
      in
      let base_content_version = column_str get_stmt 1 in
      let file_deltas =
        Relative_path.Map.mapi local_changes_saved ~f:(fun path delta ->
            match delta with
            | Modified saved -> Modified (FileInfo.from_saved path saved)
            | Deleted -> Deleted)
      in
      if Relative_path.Map.cardinal file_deltas > 0 then
        HackEventLogger.naming_sqlite_local_changes_nonempty "get";
      { base_content_version; file_deltas }
    | rc ->
      failwith
        (Printf.sprintf "Failure retrieving row: %s" (Sqlite3.Rc.to_string rc))
end

(* These are just done as modules to keep the SQLite for related tables close together. *)
module FileInfoTable = struct
  let table_name = "NAMING_FILE_INFO"

  let create_table_sqlite =
    Printf.sprintf
      "
      CREATE TABLE IF NOT EXISTS %s(
        FILE_INFO_ID INTEGER PRIMARY KEY AUTOINCREMENT,
        PATH_PREFIX_TYPE INTEGER NOT NULL,
        PATH_SUFFIX TEXT NOT NULL,
        TYPE_CHECKER_MODE INTEGER,
        DECL_HASH INTEGER,
        CLASSES TEXT,
        CONSTS TEXT,
        FUNS TEXT,
        RECS TEXT,
        TYPEDEFS TEXT
      );
      "
      table_name

  let create_index_sqlite =
    Printf.sprintf
      "
      CREATE UNIQUE INDEX IF NOT EXISTS FILE_INFO_PATH_IDX ON %s (PATH_SUFFIX, PATH_PREFIX_TYPE);
      "
      table_name

  let insert_sqlite =
    Printf.sprintf
      "
        INSERT INTO %s(
          PATH_PREFIX_TYPE,
          PATH_SUFFIX,
          TYPE_CHECKER_MODE,
          DECL_HASH,
          CLASSES,
          CONSTS,
          FUNS,
          RECS,
          TYPEDEFS
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?);
      "
      table_name

  let get_sqlite =
    Printf.sprintf
      "
        SELECT
          TYPE_CHECKER_MODE, DECL_HASH, CLASSES, CONSTS, FUNS, RECS, TYPEDEFS
        FROM %s
        WHERE PATH_PREFIX_TYPE = ? AND PATH_SUFFIX = ?;
      "
      table_name

  let iter_sqlite =
    Printf.sprintf
      "
        SELECT
          PATH_PREFIX_TYPE,
          PATH_SUFFIX,
          TYPE_CHECKER_MODE,
          DECL_HASH,
          CLASSES,
          CONSTS,
          FUNS,
          RECS,
          TYPEDEFS
        FROM %s
        ORDER BY PATH_PREFIX_TYPE, PATH_SUFFIX;
      "
      table_name

  let insert
      db
      stmt_cache
      relative_path
      ~(type_checker_mode : FileInfo.mode option)
      ~(decl_hash : Int64.t option)
      ~(classes : FileInfo.id list)
      ~(consts : FileInfo.id list)
      ~(funs : FileInfo.id list)
      ~(recs : FileInfo.id list)
      ~(typedefs : FileInfo.id list) =
    let prefix_type =
      Sqlite3.Data.INT
        (Int64.of_int
           (Relative_path.prefix_to_enum (Relative_path.prefix relative_path)))
    in
    let suffix = Sqlite3.Data.TEXT (Relative_path.suffix relative_path) in
    let type_checker_mode =
      match type_checker_mode with
      | Some type_checker_mode ->
        Sqlite3.Data.INT
          (Int64.of_int (FileInfo.mode_to_enum type_checker_mode))
      | None -> Sqlite3.Data.NULL
    in
    let decl_hash =
      match decl_hash with
      | Some decl_hash -> Sqlite3.Data.INT decl_hash
      | None -> Sqlite3.Data.NULL
    in
    let names_to_data_type names =
      let open Core_kernel in
      let names =
        String.concat ~sep:"|" (List.map names ~f:(fun (_, x, _) -> x))
      in
      match String.length names with
      | 0 -> Sqlite3.Data.NULL
      | _ -> Sqlite3.Data.TEXT names
    in
    let insert_stmt = StatementCache.make_stmt stmt_cache insert_sqlite in
    Sqlite3.bind insert_stmt 1 prefix_type |> check_rc db;

    Sqlite3.bind insert_stmt 2 suffix |> check_rc db;

    Sqlite3.bind insert_stmt 3 type_checker_mode |> check_rc db;

    Sqlite3.bind insert_stmt 4 decl_hash |> check_rc db;
    Sqlite3.bind insert_stmt 5 (names_to_data_type classes) |> check_rc db;
    Sqlite3.bind insert_stmt 6 (names_to_data_type consts) |> check_rc db;
    Sqlite3.bind insert_stmt 7 (names_to_data_type funs) |> check_rc db;
    Sqlite3.bind insert_stmt 8 (names_to_data_type recs) |> check_rc db;
    Sqlite3.bind insert_stmt 9 (names_to_data_type typedefs) |> check_rc db;
    Sqlite3.step insert_stmt |> check_rc db

  let read_row ~stmt ~path ~base_index =
    let file_mode =
      let open Option in
      Int64.to_int (column_int64 stmt base_index) >>= FileInfo.mode_of_enum
    in
    let hash = Some (column_int64 stmt (base_index + 1)) in
    let to_ids ~value ~name_type =
      match value with
      | Sqlite3.Data.TEXT s ->
        Core_kernel.(
          List.map (String.split s ~on:'|') ~f:(fun name ->
              (FileInfo.File (name_type, path), name, None)))
      | Sqlite3.Data.NULL -> []
      | _ -> failwith "Unexpected column type when retrieving names"
    in
    let classes =
      to_ids
        ~value:(Sqlite3.column stmt (base_index + 2))
        ~name_type:FileInfo.Class
    in
    let consts =
      to_ids
        ~value:(Sqlite3.column stmt (base_index + 3))
        ~name_type:FileInfo.Const
    in
    let funs =
      to_ids
        ~value:(Sqlite3.column stmt (base_index + 4))
        ~name_type:FileInfo.Fun
    in
    let record_defs =
      to_ids
        ~value:(Sqlite3.column stmt (base_index + 5))
        ~name_type:FileInfo.RecordDef
    in
    let typedefs =
      to_ids
        ~value:(Sqlite3.column stmt (base_index + 6))
        ~name_type:FileInfo.Typedef
    in
    FileInfo.
      {
        hash;
        file_mode;
        funs;
        classes;
        record_defs;
        typedefs;
        consts;
        comments = None;
      }

  let get_file_info db stmt_cache path =
    let get_stmt = StatementCache.make_stmt stmt_cache get_sqlite in
    let prefix_type =
      Relative_path.prefix_to_enum (Relative_path.prefix path)
    in
    let suffix = Relative_path.suffix path in
    Sqlite3.bind get_stmt 1 (Sqlite3.Data.INT (Int64.of_int prefix_type))
    |> check_rc db;
    Sqlite3.bind get_stmt 2 (Sqlite3.Data.TEXT suffix) |> check_rc db;
    match Sqlite3.step get_stmt with
    | Sqlite3.Rc.DONE -> None
    | Sqlite3.Rc.ROW -> Some (read_row ~stmt:get_stmt ~path ~base_index:0)
    | rc ->
      failwith
        (Printf.sprintf "Failure retrieving row: %s" (Sqlite3.Rc.to_string rc))

  let fold stmt_cache ~init ~f =
    let iter_stmt = StatementCache.make_stmt stmt_cache iter_sqlite in
    let f iter_stmt acc =
      let prefix_int = column_int64 iter_stmt 0 in
      let suffix = column_str iter_stmt 1 in
      let path = make_relative_path ~prefix_int ~suffix in
      let fi = read_row ~stmt:iter_stmt ~path ~base_index:2 in
      f path fi acc
    in
    fold_sqlite iter_stmt ~f ~init
end

module SymbolTable = struct
  let table_name = "NAMING_SYMBOLS"

  let create_table_sqlite =
    (* CANON_HASH is the hash of the lowercase form of the name (even for consts which don't need it) *)
    (* FLAGS comes from Naming_types.name_kind *)
    Printf.sprintf
      "
      CREATE TABLE IF NOT EXISTS %s(
        HASH INTEGER PRIMARY KEY NOT NULL,
        CANON_HASH INTEGER NOT NULL,
        DECL_HASH INTEGER NOT NULL,
        FLAGS INTEGER NOT NULL,
        FILE_INFO_ID INTEGER NOT NULL
      );
    "
      table_name

  let create_index_sqlite =
    Printf.sprintf
      "
    CREATE INDEX IF NOT EXISTS TYPES_CANON ON %s (CANON_HASH);
    "
      table_name

  let insert_sqlite =
    Printf.sprintf
      "
      INSERT INTO %s(
        HASH,
        CANON_HASH,
        DECL_HASH,
        FLAGS,
        FILE_INFO_ID)
      VALUES (?, ?, ?, ?, ?);
    "
      table_name

  let (get_sqlite, get_sqlite_case_insensitive) =
    let base =
      Str.global_replace
        (Str.regexp "{table_name}")
        table_name
        "
        SELECT
          NAMING_FILE_INFO.PATH_PREFIX_TYPE,
          NAMING_FILE_INFO.PATH_SUFFIX,
          {table_name}.FLAGS
        FROM {table_name}
        LEFT JOIN NAMING_FILE_INFO ON
          {table_name}.FILE_INFO_ID = NAMING_FILE_INFO.FILE_INFO_ID
        WHERE {table_name}.{hash} = ?"
    in
    ( Str.global_replace (Str.regexp "{hash}") "HASH" base,
      Str.global_replace (Str.regexp "{hash}") "CANON_HASH" base )

  (** Note: name parameter is used solely for debugging; it's only the hash and canon_hash that get inserted. *)
  let insert
      db stmt_cache ~name ~hash ~canon_hash ~name_kind ~file_info_id ~decl_hash
      : (unit, insertion_error) result =
    let flags = name_kind |> Naming_types.name_kind_to_enum |> Int64.of_int in
    let insert_stmt = StatementCache.make_stmt stmt_cache insert_sqlite in
    Sqlite3.bind insert_stmt 1 (Sqlite3.Data.INT hash) |> check_rc db;
    Sqlite3.bind insert_stmt 2 (Sqlite3.Data.INT canon_hash) |> check_rc db;
    Sqlite3.bind insert_stmt 3 (Sqlite3.Data.INT decl_hash) |> check_rc db;
    Sqlite3.bind insert_stmt 4 (Sqlite3.Data.INT flags) |> check_rc db;
    Sqlite3.bind insert_stmt 5 (Sqlite3.Data.INT file_info_id) |> check_rc db;
    insert_safe ~name ~name_kind ~hash ~canon_hash @@ fun () ->
    Sqlite3.step insert_stmt |> check_rc db

  let get db stmt_cache dep stmt =
    let hash = dep |> Typing_deps.Dep.to_int64 in
    let get_stmt = StatementCache.make_stmt stmt_cache stmt in
    Sqlite3.bind get_stmt 1 (Sqlite3.Data.INT hash) |> check_rc db;
    match Sqlite3.step get_stmt with
    | Sqlite3.Rc.DONE -> None
    | Sqlite3.Rc.ROW ->
      let prefix_type = column_int64 get_stmt 0 in
      let suffix = column_str get_stmt 1 in
      let flag = Option.value_exn (column_int64 get_stmt 2 |> Int64.to_int) in
      let name_kind =
        Option.value_exn (flag |> Naming_types.name_kind_of_enum)
      in
      Some (make_relative_path ~prefix_int:prefix_type ~suffix, name_kind)
    | rc ->
      failwith
        (Printf.sprintf "Failure retrieving row: %s" (Sqlite3.Rc.to_string rc))
end

let db_cache :
    [ `Not_yet_cached
    | `Cached of (db_path * Sqlite3.db * StatementCache.t) option
    ]
    ref =
  ref `Not_yet_cached

let open_db (Db_path path) : Sqlite3.db =
  let db = Sqlite3.db_open path in
  Sqlite3.exec db "PRAGMA synchronous = OFF;" |> check_rc db;
  Sqlite3.exec db "PRAGMA journal_mode = MEMORY;" |> check_rc db;
  db

let get_db_and_stmt_cache (path : db_path) : Sqlite3.db * StatementCache.t =
  match !db_cache with
  | `Cached (Some (existing_path, db, stmt_cache))
    when equal_db_path path existing_path ->
    (db, stmt_cache)
  | _ ->
    let db = open_db path in
    let stmt_cache = StatementCache.make ~db in
    db_cache := `Cached (Some (path, db, stmt_cache));
    (db, stmt_cache)

let validate_can_open_db (db_path : db_path) : unit =
  let (_ : Sqlite3.db * StatementCache.t) = get_db_and_stmt_cache db_path in
  ()

let free_db_cache () : unit = db_cache := `Not_yet_cached

let save_file_info db stmt_cache relative_path file_info :
    int * insertion_error list =
  let open Core_kernel in
  FileInfoTable.insert
    db
    stmt_cache
    relative_path
    ~type_checker_mode:file_info.FileInfo.file_mode
    ~decl_hash:file_info.FileInfo.hash
    ~consts:file_info.FileInfo.consts
    ~classes:file_info.FileInfo.classes
    ~funs:file_info.FileInfo.funs
    ~recs:file_info.FileInfo.record_defs
    ~typedefs:file_info.FileInfo.typedefs;
  let file_info_id = ref None in
  Sqlite3.exec_not_null_no_headers
    db
    "SELECT last_insert_rowid()"
    ~cb:(fun row ->
      match row with
      | [| row_id |] -> file_info_id := Some (Int64.of_string row_id)
      | [||] -> failwith "Got no columns when querying last inserted row ID"
      | _ -> failwith "Got too many columns when querying last inserted row ID")
  |> check_rc db;
  let file_info_id =
    match !file_info_id with
    | Some id -> id
    | None -> failwith "Could not get last inserted row ID"
  in
  let insert ~name_kind ~dep_ctor (symbols_inserted, errors) (_, name, decl_hash)
      =
    let decl_hash = Option.value decl_hash ~default:Int64.zero in
    let hash =
      name
      |> dep_ctor
      |> Typing_deps.Dep.make Typing_deps.Mode.Hash64Bit
      |> Typing_deps.Dep.to_int64
    in
    let canon_hash =
      name
      |> to_canon_name_key
      |> dep_ctor
      |> Typing_deps.Dep.make Typing_deps.Mode.Hash64Bit
      |> Typing_deps.Dep.to_int64
    in
    match
      SymbolTable.insert
        db
        stmt_cache
        ~name
        ~name_kind
        ~hash
        ~canon_hash
        ~file_info_id
        ~decl_hash
    with
    | Ok () -> (symbols_inserted + 1, errors)
    | Error error -> (symbols_inserted, error :: errors)
  in
  let results = (0, []) in
  let results =
    List.fold
      file_info.FileInfo.funs
      ~init:results
      ~f:
        (insert ~name_kind:Naming_types.Fun_kind ~dep_ctor:(fun name ->
             Typing_deps.Dep.Fun name))
  in
  let results =
    List.fold
      file_info.FileInfo.classes
      ~init:results
      ~f:
        (insert
           ~name_kind:Naming_types.(Type_kind TClass)
           ~dep_ctor:(fun name -> Typing_deps.Dep.Type name))
  in
  let results =
    List.fold
      file_info.FileInfo.typedefs
      ~init:results
      ~f:
        (insert
           ~name_kind:Naming_types.(Type_kind TTypedef)
           ~dep_ctor:(fun name -> Typing_deps.Dep.Type name))
  in
  let results =
    List.fold
      file_info.FileInfo.record_defs
      ~init:results
      ~f:
        (insert
           ~name_kind:Naming_types.(Type_kind TRecordDef)
           ~dep_ctor:(fun name -> Typing_deps.Dep.Type name))
  in
  let results =
    List.fold
      file_info.FileInfo.consts
      ~init:results
      ~f:
        (insert ~name_kind:Naming_types.Const_kind ~dep_ctor:(fun name ->
             Typing_deps.Dep.GConst name))
  in
  results

let save_file_infos db_name file_info_map ~base_content_version =
  let db = Sqlite3.db_open db_name in
  let stmt_cache = StatementCache.make ~db in
  Sqlite3.exec db "BEGIN TRANSACTION;" |> check_rc db;
  try
    Sqlite3.exec db LocalChanges.create_table_sqlite |> check_rc db;
    Sqlite3.exec db FileInfoTable.create_table_sqlite |> check_rc db;
    Sqlite3.exec db SymbolTable.create_table_sqlite |> check_rc db;

    (* Incremental updates only update the single row in this table, so we need
     * to write in some dummy data to start. *)
    LocalChanges.prime db stmt_cache base_content_version;
    let save_result =
      Relative_path.Map.fold
        file_info_map
        ~init:empty_save_result
        ~f:(fun path fi acc ->
          let (symbols_added, errors) = save_file_info db stmt_cache path fi in
          {
            files_added = acc.files_added + 1;
            symbols_added = acc.symbols_added + symbols_added;
            errors = List.rev_append acc.errors errors;
          })
    in
    Sqlite3.exec db FileInfoTable.create_index_sqlite |> check_rc db;
    Sqlite3.exec db SymbolTable.create_index_sqlite |> check_rc db;
    Sqlite3.exec db "END TRANSACTION;" |> check_rc db;
    StatementCache.close stmt_cache;
    if not @@ Sqlite3.db_close db then
      failwith @@ Printf.sprintf "Could not close database at %s" db_name;
    save_result
  with
  | e ->
    Sqlite3.exec db "END TRANSACTION;" |> check_rc db;
    raise e

let copy_and_update
    ~(existing_db : db_path) ~(new_db : db_path) (local_changes : local_changes)
    : unit =
  let (Db_path existing_path, Db_path new_path) = (existing_db, new_db) in
  FileUtil.cp ~force:(FileUtil.Ask (fun _ -> false)) [existing_path] new_path;
  let new_db = open_db new_db in
  let stmt_cache = StatementCache.make ~db:new_db in
  LocalChanges.update new_db stmt_cache local_changes;
  StatementCache.close stmt_cache;
  ()

let get_local_changes (db_path : db_path) : local_changes =
  let (_db, stmt_cache) = get_db_and_stmt_cache db_path in
  LocalChanges.get stmt_cache

let fold
    ~(warn_on_naming_costly_iter : bool)
    ~(db_path : db_path)
    ~init
    ~f
    ~file_deltas =
  let start_t = Unix.gettimeofday () in
  (* We depend on [Relative_path.Map.bindings] returning results in increasing
   * order here. *)
  let sorted_changes = Relative_path.Map.bindings file_deltas in
  (* This function looks kind of funky, but it's not too terrible when
   * explained. We essentially have two major inputs:
   *   1. a path and file info that we got from SQLite. In order for this
   *      function to work properly, we require that consecutive invocations
   *      of this function within the same [fold] call have paths that are
   *      strictly increasing.
   *   2. the list of local changes. This list is also required to be in
   *      increasing sorted order.
   * In order to make sure that we a) return results in sorted order, and
   * b) properly handle files that have been added (and therefore will never
   * show up in the [path] and [fi] args), we essentially walk both lists in
   * sorted order (walking the SQLite list through successive invocations and
   * walking the local changes list through recursion), at each step folding
   * over whichever entry has the lowest sort order. If both paths are
   * identical, we use the one from our local changes. In this way we get O(n)
   * iteration while handling additions, modifications, and deletions
   * properly (again, given our input sorting restraints). *)
  let rec consume_sorted_changes path fi (sorted_changes, acc) =
    match sorted_changes with
    | hd :: tl when Relative_path.compare (fst hd) path < 0 ->
      begin
        match snd hd with
        | Modified local_fi ->
          consume_sorted_changes path fi (tl, f (fst hd) local_fi acc)
        | Deleted -> consume_sorted_changes path fi (tl, acc)
      end
    | hd :: tl when Relative_path.equal (fst hd) path ->
      begin
        match snd hd with
        | Modified fi -> (tl, f path fi acc)
        | Deleted -> (tl, acc)
      end
    | _ -> (sorted_changes, f path fi acc)
  in
  let (_db, stmt_cache) = get_db_and_stmt_cache db_path in
  let (remaining_changes, acc) =
    FileInfoTable.fold
      stmt_cache
      ~init:(sorted_changes, init)
      ~f:consume_sorted_changes
  in
  let acc =
    List.fold_left
      ~f:
        begin
          fun acc (path, delta) ->
          match delta with
          | Modified fi -> f path fi acc
          | Deleted -> (* This probably shouldn't happen? *) acc
        end
      ~init:acc
      remaining_changes
  in
  if warn_on_naming_costly_iter then begin
    Hh_logger.log
      "NAMING_COSTLY_ITER\n%s"
      (Exception.get_current_callstack_string 99 |> Exception.clean_stack);
    HackEventLogger.naming_costly_iter ~start_t
  end;
  acc

let get_file_info (db_path : db_path) path =
  let (db, stmt_cache) = get_db_and_stmt_cache db_path in
  FileInfoTable.get_file_info db stmt_cache path

(* Same as `get_db_and_stmt_cache` but with logging for when opening the
database fails. *)
let sqlite_exn_wrapped_get_db_and_stmt_cache db_path name =
  try get_db_and_stmt_cache db_path with
  | Sqlite3.Error _ as exn ->
    Hh_logger.info
      "Sqlite_db_open_bug: couldn't open the DB at `%s` while getting the position of `%s`"
      (show_db_path db_path)
      name;
    raise exn

let get_type_wrapper
    (result : (Relative_path.t * Naming_types.name_kind) option) :
    (Relative_path.t * Naming_types.kind_of_type) option =
  match result with
  | None -> None
  | Some (filename, Naming_types.Type_kind kind_of_type) ->
    Some (filename, kind_of_type)
  | Some (_, _) -> failwith "wrong symbol kind"

let get_fun_wrapper (result : (Relative_path.t * Naming_types.name_kind) option)
    : Relative_path.t option =
  match result with
  | None -> None
  | Some (filename, Naming_types.Fun_kind) -> Some filename
  | Some (_, _) -> failwith "wrong symbol kind"

let get_const_wrapper
    (result : (Relative_path.t * Naming_types.name_kind) option) :
    Relative_path.t option =
  match result with
  | None -> None
  | Some (filename, Naming_types.Const_kind) -> Some filename
  | Some (_, _) -> failwith "wrong symbol kind"

let get_type_path_by_name (db_path : db_path) name =
  let (db, stmt_cache) =
    sqlite_exn_wrapped_get_db_and_stmt_cache db_path name
  in
  SymbolTable.get
    db
    stmt_cache
    (Typing_deps.Dep.Type name
    |> Typing_deps.Dep.make Typing_deps.Mode.Hash64Bit)
    SymbolTable.get_sqlite
  |> get_type_wrapper

let get_itype_path_by_name (db_path : db_path) name =
  let (db, stmt_cache) =
    sqlite_exn_wrapped_get_db_and_stmt_cache db_path name
  in
  SymbolTable.get
    db
    stmt_cache
    (Typing_deps.Dep.Type (Caml.String.lowercase_ascii name)
    |> Typing_deps.Dep.make Typing_deps.Mode.Hash64Bit)
    SymbolTable.get_sqlite_case_insensitive
  |> get_type_wrapper

let get_fun_path_by_name (db_path : db_path) name =
  let (db, stmt_cache) = get_db_and_stmt_cache db_path in
  SymbolTable.get
    db
    stmt_cache
    (Typing_deps.Dep.Fun name |> Typing_deps.Dep.make Typing_deps.Mode.Hash64Bit)
    SymbolTable.get_sqlite
  |> get_fun_wrapper

let get_ifun_path_by_name (db_path : db_path) name =
  let (db, stmt_cache) = get_db_and_stmt_cache db_path in
  SymbolTable.get
    db
    stmt_cache
    (Typing_deps.Dep.Fun (Caml.String.lowercase_ascii name)
    |> Typing_deps.Dep.make Typing_deps.Mode.Hash64Bit)
    SymbolTable.get_sqlite_case_insensitive
  |> get_fun_wrapper

let get_const_path_by_name (db_path : db_path) name =
  let (db, stmt_cache) = get_db_and_stmt_cache db_path in
  SymbolTable.get
    db
    stmt_cache
    (Typing_deps.Dep.GConst name
    |> Typing_deps.Dep.make Typing_deps.Mode.Hash64Bit)
    SymbolTable.get_sqlite
  |> get_const_wrapper

let get_path_by_64bit_dep (db_path : db_path) (dep : Typing_deps.Dep.t) =
  let (db, stmt_cache) = get_db_and_stmt_cache db_path in
  SymbolTable.get db stmt_cache dep SymbolTable.get_sqlite
