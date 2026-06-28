/* wasm32-wasip1 wrapper: map sigsetjmp/siglongjmp onto setjmp/longjmp.
 * LLVM's wasm SJLJ lowering (-mllvm -wasm-enable-sjlj) only recognizes calls
 * named setjmp/longjmp; sigsetjmp stays an undefined extern otherwise.
 * WASI has no signal masks, and PostgreSQL always calls sigsetjmp(env, 0)
 * (savemask=0), so the mapping is semantically exact. */
#pragma once
#include_next <setjmp.h>
#undef sigsetjmp
#define sigsetjmp(env, savemask) setjmp(env)
#undef siglongjmp
#define siglongjmp(env, val) longjmp(env, val)
