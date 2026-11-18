#include "types.hpp"
namespace storage::detail {
Worker::Worker(int value) : value(value) {}
int Worker::handle(int n) { return value + n; }
int Worker::handle(double n) { return value + static_cast<int>(n); }
bool Worker::operator==(const Worker& other) const { return value == other.value; }
int Worker::create() { return 1; }
int out_of_line(int n) { return n + 1; }
int dispatch() { return 2; }
}
