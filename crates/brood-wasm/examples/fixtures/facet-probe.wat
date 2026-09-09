(module
  (memory (export "memory") 32)
  ;; Prototype-only ABI: checksum input bytes, emit a fixed 16-byte draw header,
  ;; and return output length in the high word plus checksum in the low word.
  (func (export "facet-render") (param $input i32) (param $len i32) (param $output i32) (result i64)
    (local $cursor i32) (local $sum i32)
    (local.set $cursor (local.get $input))
    (loop $scan
      (if (i32.lt_u (local.get $cursor) (i32.add (local.get $input) (local.get $len)))
        (then
          (local.set $sum (i32.add (local.get $sum) (i32.load8_u (local.get $cursor))))
          (local.set $cursor (i32.add (local.get $cursor) (i32.const 1)))
          (br $scan))))
    (i64.store (local.get $output) (i64.extend_i32_u (local.get $sum)))
    (i64.store offset=8 (local.get $output) (i64.extend_i32_u (local.get $len)))
    (i64.or
      (i64.shl (i64.const 16) (i64.const 32))
      (i64.extend_i32_u (local.get $sum)))))
