(module
  (import "brood:actor/message" "send" (func $send (param i32 i32)))
  (memory (export "memory") 1)
  (func (export "facet-render") (param i32 i32 i32) (result i64) (i64.const 0)))
