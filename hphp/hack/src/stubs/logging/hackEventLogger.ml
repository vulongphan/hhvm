(*
 * Copyright (c) 2015, Facebook, Inc.
 * All rights reserved.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the "hack" directory of this source tree.
 *
 *)

type serialized_globals = Serialized_globals

let serialize_globals () = Serialized_globals

type rollout_flags = {
  use_direct_decl_parser: bool;
  longlived_workers: bool;
  max_times_to_defer_type_checking: int option;
  small_buckets_for_dirty_names: bool;
  symbolindex_search_provider: string;
  require_saved_state: bool;
  stream_errors: bool;
  use_direct_decl_in_tc_loop: bool;
  deferments_light: bool;
  old_naming_table_for_redecl: bool;
  log_from_client_when_slow_monitor_connections: bool;
  naming_sqlite_in_hack_64: bool;
}

let flush () = ()

let deserialize_globals _ = ()

let set_use_watchman _ = ()

let set_use_full_fidelity_parser _ = ()

let set_lazy_incremental _ = ()

let set_search_chunk_size _ = ()

let set_changed_mergebase _ = ()

let set_from _ = ()

let set_hhconfig_version _ = ()

let set_rollout_flags _ = ()

let typechecker_exit _ _ _ ~is_oom:_ = ()

let init
    ~root:_
    ~hhconfig_version:_
    ~init_id:_
    ~custom_columns:_
    ~informant_managed:_
    ~rollout_flags:_
    ~time:_
    ~max_workers:_
    ~profile_owner:_
    ~profile_desc:_ =
  ()

let init_worker
    ~root:_
    ~hhconfig_version:_
    ~init_id:_
    ~custom_columns:_
    ~rollout_flags:_
    ~time:_
    ~profile_owner:_
    ~profile_desc:_ =
  ()

let init_monitor
    ~from:_
    ~custom_columns:_
    ~proc_stack:_
    ~hhconfig_version:_
    ~rollout_flags:_
    _
    _
    _ =
  ()

let init_batch_tool ~init_id:_ ~root:_ ~time:_ = ()

let starting_first_server _ = ()

let refuse_to_restart_server ~reason:_ ~server_state:_ ~version_matches:_ = ()

let server_receipt_to_monitor_write_exn ~server_receipt_to_monitor_file:_ _ = ()

let server_receipt_to_monitor_read_exn ~server_receipt_to_monitor_file:_ _ _ =
  ()

let init_lazy_end
    _
    ~state_distance:_
    ~approach_name:_
    ~init_error:_
    ~init_error_stack:_
    ~init_type:_ =
  ()

let server_is_partially_ready () = ()

let server_is_ready _ = ()

let load_deptable_end _ = ()

let saved_state_download_and_load_done
    ~load_state_approach:_
    ~success:_
    ~state_result:_
    ~load_state_natively_64bit:_
    _ =
  ()

let tried_to_be_hg_aware_with_precomputed_saved_state_warning _ = ()

let init_start ~experiments_config_meta = ignore experiments_config_meta

let nfs_root _ = ()

let load_state_worker_end ~is_cached:_ _ _ = ()

let vcs_changed_files_end _ _ = ()

let type_check_dirty ~start_t:_ ~dirty_count:_ ~recheck_count:_ = ()

let out_of_date _ = ()

let lock_stolen _ = ()

let client_init ~init_id:_ ~custom_columns:_ _ = ()

let serverless_ide_init ~init_id:_ = ()

let client_set_mode _ = ()

let serverless_ide_set_root _ = ()

let client_start _ = ()

let client_stop _ = ()

let client_restart ~data:_ = ()

let client_check_start () = ()

let client_check _ _ = ()

let client_lsp_method_handled
    ~root:_
    ~method_:_
    ~kind:_
    ~path_opt:_
    ~result_count:_
    ~result_extra_telemetry:_
    ~tracking_id:_
    ~start_queue_time:_
    ~start_hh_server_state:_
    ~start_handle_time:_
    ~serverless_ide_flag:_ =
  ()

let client_lsp_method_exception
    ~root:_
    ~method_:_
    ~kind:_
    ~path_opt:_
    ~tracking_id:_
    ~start_queue_time:_
    ~start_hh_server_state:_
    ~start_handle_time:_
    ~serverless_ide_flag:_
    ~message:_
    ~data_opt:_
    ~source:_ =
  ()

let serverless_ide_bug ~message:_ ~data:_ = ()

let client_lsp_exception ~root:_ ~message:_ ~data_opt:_ ~source:_ = ()

let serverless_ide_startup ~component:_ ~start_time:_ = ()

let serverless_ide_local_files ~local_file_count:_ = ()

let serverless_ide_destroy_ok _ = ()

let serverless_ide_destroy_error _ _ _ = ()

let server_hung_up
    ~external_exit_status:_
    ~underlying_exit_status:_
    ~client_exn:_
    ~client_stack:_
    ~server_stack:_
    ~server_msg:_ =
  ()

let client_bad_exit ~command_name:_ _ _ = ()

let glean_globalrev_supplied ~globalrev:_ = ()

let glean_globalrev_from_hg ~globalrev:_ ~start_time:_ = ()

let glean_globalrev_error _ = ()

let glean_init _ ~start_time:_ = ()

let glean_init_failure _ ~stack:_ = ()

let glean_fetch_namespaces ~count:_ ~start_time:_ = ()

let glean_fetch_namespaces_error _ = ()

let ranked_autocomplete_duration ~start_time:_ = ()

let ranked_autocomplete_request_duration ~start_time:_ = ()

let monitor_dead_but_typechecker_alive () = ()

let client_established_connection _ = ()

let client_establish_connection_exception _ = ()

let client_connect_once ~t_start:_ = ()

let client_connect_once_failure ~t_start:_ _ = ()

let client_connect_to_monitor_slow_log () = ()

let client_connect_autostart () = ()

let check_response _ = ()

let got_client_channels _ = ()

let get_client_channels_exception _ = ()

let got_persistent_client_channels _ = ()

let get_persistent_client_channels_exception _ = ()

let handled_connection _ = ()

let handle_connection_exception _ _ = ()

let handled_persistent_connection _ = ()

let handle_persistent_connection_exception _ _ ~is_fatal:_ = ()

let handled_command
    _ ~start_t:_ ~major_gc_time:_ ~minor_gc_time:_ ~parsed_files:_ =
  ()

let remote_scheduler_get_dirty_files_end _ _ = ()

let remote_scheduler_update_dependency_graph_end ~edges:_ _ = ()

let remote_scheduler_save_naming_end _ = ()

let credentials_check_failure _ _ = ()

let credentials_check_end _ _ = ()

let remote_worker_type_check_end _ = ()

let remote_worker_load_naming_end _ = ()

let recheck_end _ _ _ _ _ = ()

let indexing_end ~desc:_ _ = ()

let parsing_end _ _ ~parsed_count:_ = ()

let parsing_end_for_init _ _ ~parsed_count:_ ~desc:_ = ()

let parsing_end_for_typecheck _ _ ~parsed_count:_ = ()

let updating_deps_end ~count:_ ~desc:_ ~start_t:_ = ()

let naming_costly_iter ~start_t:_ = ()

let naming_end ~count:_ _ _ = ()

let global_naming_end ~count:_ ~desc:_ ~heap_size:_ ~start_t:_ = ()

let run_search_end _ = ()

let update_search_end _ _ = ()

let naming_from_saved_state_end _ = ()

let naming_sqlite_local_changes_nonempty _ = ()

let type_decl_end _ = ()

let first_redecl_end _ _ = ()

let second_redecl_end _ _ = ()

let type_check_primary_position_bug ~current_file:_ ~message:_ ~stack:_ = ()

let type_check_exn_bug ~typechecking_is_deferring:_ ~path:_ ~pos:_ ~e:_ = ()

let invariant_violation_bug
    ~typechecking_is_deferring:_ ~path:_ ~pos:_ ~desc:_ _ =
  ()

let type_check_end
    _
    ~heap_size:_
    ~started_count:_
    ~count:_
    ~adhoc_profiling:_
    ~desc:_
    ~experiments:_
    ~start_t:_ =
  ()

let notifier_returned _ _ = ()

let load_state_exn _ = ()

let prechecked_update_rechecked _ = ()

let prechecked_evaluate_init _ _ = ()

let prechecked_evaluate_incremental _ _ = ()

let check_mergebase_failed _ _ = ()

let check_mergebase_success _ = ()

let type_at_pos_batch ~start_time:_ ~num_files:_ ~num_positions:_ ~results:_ =
  ()

let with_id ~stage:_ _ f = f ()

let with_rechecked_stats _ _ _ f = f ()

let with_init_type _ f = f ()

let with_check_kind _ f = f ()

let state_loader_dirty_files _ = ()

let changed_while_parsing_end _ = ()

let save_decls_end _ _ = ()

let save_decls_failure _ = ()

let load_decls_end _ = ()

let load_decls_failure _ _ = ()

let saved_state_load_ok _ ~start_time:_ = ()

let saved_state_load_failure _ ~start_time:_ = ()

let saved_state_dirty_files_ok ~start_time:_ = ()

let saved_state_dirty_files_failure _ ~start_time:_ = ()

(** Informant events *)
let init_informant_prefetcher_runner _ = ()

let informant_decision_on_saved_state
    ~start_t:_ ~state_distance:_ ~incremental_distance:_ =
  ()

let informant_induced_kill _ = ()

let informant_induced_restart _ = ()

let informant_no_xdb_result _ = ()

let informant_prefetcher_success _ = ()

let informant_prefetcher_failed _ _ = ()

let informant_prefetcher_timed_out _ = ()

let informant_state_leave _ = ()

let find_svn_rev_failed _ _ = ()

let find_svn_rev_success _ = ()

let find_xdb_match_failed _ _ = ()

let find_xdb_match_success _ = ()

let find_xdb_match_timed_out _ = ()

let informant_find_saved_state_failed _ = ()

let informant_find_saved_state_success ~distance:_ _ = ()

let revision_tracker_init_svn_rev_failed _ = ()

let xdb_malformed_result _ = ()

(** Watchman Event Watcher client running in the informant *)
let informant_watcher_not_available _ = ()

let informant_watcher_unknown_state _ = ()

let informant_watcher_mid_update_state _ = ()

let informant_watcher_settled_state _ = ()

let informant_watcher_starting_server_from_settling _ = ()

(** Server Monitor events *)
let accepting_on_socket_exception _ = ()

let malformed_build_id _ = ()

let send_fd_failure _ = ()

let typechecker_already_running _ = ()

(** Watchman Event Watcher events *)
let init_watchman_event_watcher _ _ = ()

let init_watchman_failed _ = ()

let restarting_watchman_subscription _ = ()

let watchman_uncaught_exception _ = ()

let monitor_giving_up_exception _ = ()

let processed_clients _ = ()

let search_symbol_index
    ~query_text:_
    ~max_results:_
    ~results:_
    ~kind_filter:_
    ~duration:_
    ~actype:_
    ~caller:_
    ~search_provider:_ =
  ()

let shallow_decl_errors_emitted _ = ()

let server_progress_write_exn ~server_progress_file:_ _ = ()

let server_progress_read_exn ~server_progress_file:_ _ = ()

let worker_exception _ = ()

module ProfileTypeCheck = struct
  let process_file ~recheck_id:_ ~path:_ ~telemetry:_ = ()

  let compute_tast ~path:_ ~telemetry:_ = ()

  let get_telemetry_url ~init_id:_ ~recheck_id:_ = ""
end

module CGroup = struct
  let profile
      ~cgroup:_ ~event:_ ~stage:_ ~metric:_ ~start:_ ~delta:_ ~hwm_delta:_ =
    ()
end

module ReHulk = struct
  let profile
      ~recheck_id:_
      ~start_time:_
      ~action:_
      ~re_worker:_
      ~queued_duration:_
      ~input_upload_duration:_
      ~input_fetch_duration:_
      ~output_upload_duration:_
      ~output_fetch_duration:_
      ~worker_duration:_
      ~execution_duration:_ =
    ()
end

module Memory = struct
  let profile_if_needed () = ()
end

module ProfileDecl = struct
  let count_decl
      ~kind:_
      ~cpu_duration:_
      ~decl_id:_
      ~decl_name:_
      ~decl_origin:_
      ~decl_file:_
      ~decl_callstack:_
      ~decl_start_time:_
      ~subdecl_member_name:_
      ~subdecl_eagerness:_
      ~subdecl_callstack:_
      ~subdecl_start_time:_ =
    ()
end

module Rage = struct
  let rage_start ~rageid:_ ~desc:_ ~root:_ ~from:_ ~disk_config:_ = ()

  let rage
      ~rageid:_
      ~desc:_
      ~root:_
      ~from:_
      ~hhconfig_version:_
      ~disk_config:_
      ~experiments:_
      ~experiments_config_meta:_
      ~items:_
      ~result:_
      ~start_time:_ =
    ()

  let get_telemetry_url ~(rageid : string) : string = rageid
end
