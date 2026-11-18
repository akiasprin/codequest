#include "events.hpp"
void emit(int) {}
void Writer::write(int v) { emit(v); }
void run(Sink& sink) {
 emit(1);
 emit(2);
 auto callback = &emit;
 callback(3);
 auto deferred = [] { emit(4); };
 deferred();
 sink.write(5);
}
