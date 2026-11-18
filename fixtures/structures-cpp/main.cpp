#include "types.hpp"
static int local_helper() { return 10; }
namespace sibling { int dispatch() { return 99; } }
int run() {
  storage::detail::Worker worker(1);
  storage::detail::Box<int> box{3};
  auto deferred = [] { return sibling::dispatch(); };
  (void)deferred;
  return worker.handle(1) + worker.handle(2.0) + storage::detail::dispatch()
      + storage::detail::out_of_line(box.get()) + local_helper();
}
int dynamic_call(storage::detail::Base& value) { return value.handle(1); }
void method_value() { auto callback = &storage::detail::dispatch; (void)callback; }
const char* fake_code = "int phantom() { return sibling::dispatch(); }";
#if defined(NEVER_ENABLED)
int excluded() { return 0; }
#endif
