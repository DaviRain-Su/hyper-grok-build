;; safe-shell-wasm guest (Phase 1 core-wasm bootstrap ABI)
;;
;; Exports:
;;   hyper_ext_abi_version      () -> i32  (= 1)
;;   hyper_ext_on_session_start () -> i32  (= 0)
;;   hyper_ext_on_pre_tool_use  () -> i32  (0 allow / 1 deny)
;;
;; Imports (hyper_host):
;;   input_len  () -> i32
;;   input_byte (i32) -> i32
;;
;; Denies when the tool input JSON contains the ASCII substring "rm -rf".
;;
;; Build (requires wabt or wat crate):
;;   wat2wasm extension.wat -o extension.wasm
;;
;; Install for local use (trusted user plugins dir):
;;   mkdir -p ~/.grok/plugins/safe-shell-wasm
;;   cp plugin.json extension.wasm ~/.grok/plugins/safe-shell-wasm/
;;   # enable in config.toml: [plugins] enabled = ["safe-shell-wasm"]

(module
  (import "hyper_host" "input_len" (func $input_len (result i32)))
  (import "hyper_host" "input_byte" (func $input_byte (param i32) (result i32)))

  (func (export "hyper_ext_abi_version") (result i32)
    i32.const 1)

  (func (export "hyper_ext_on_session_start") (result i32)
    i32.const 0)

  (func (export "hyper_ext_on_pre_tool_use") (result i32)
    (local $i i32)
    (local $n i32)
    (local $b0 i32) (local $b1 i32) (local $b2 i32)
    (local $b3 i32) (local $b4 i32) (local $b5 i32)
    (local.set $n (call $input_len))
    (local.set $i (i32.const 0))
    (block $done
      (loop $scan
        (br_if $done (i32.ge_s (local.get $i) (local.get $n)))
        ;; "rm -rf" = 72 6d 20 2d 72 66
        (local.set $b0 (call $input_byte (local.get $i)))
        (local.set $b1 (call $input_byte (i32.add (local.get $i) (i32.const 1))))
        (local.set $b2 (call $input_byte (i32.add (local.get $i) (i32.const 2))))
        (local.set $b3 (call $input_byte (i32.add (local.get $i) (i32.const 3))))
        (local.set $b4 (call $input_byte (i32.add (local.get $i) (i32.const 4))))
        (local.set $b5 (call $input_byte (i32.add (local.get $i) (i32.const 5))))
        (if (i32.and
              (i32.and
                (i32.and (i32.eq (local.get $b0) (i32.const 0x72))
                         (i32.eq (local.get $b1) (i32.const 0x6d)))
                (i32.and (i32.eq (local.get $b2) (i32.const 0x20))
                         (i32.eq (local.get $b3) (i32.const 0x2d))))
              (i32.and (i32.eq (local.get $b4) (i32.const 0x72))
                       (i32.eq (local.get $b5) (i32.const 0x66))))
          (then (return (i32.const 1))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $scan)
      )
    )
    i32.const 0
  )
)
