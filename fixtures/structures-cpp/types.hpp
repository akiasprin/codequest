#pragma once
namespace storage::detail {
struct Base { virtual int handle(int) = 0; virtual ~Base() = default; };
class Worker final : public Base {
 public:
  explicit Worker(int value);
  int handle(int value) override;
  int handle(double value);
  bool operator==(const Worker& other) const;
  static int create();
  class Nested;
  int value;
};
class Worker::Nested { public: int inner; };
template<class T> struct Box { T value; T get() const { return value; } };
template<> struct Box<int> { int value; int get() const { return value + 1; } };
enum class Event { Ready, Done };
union Packet { unsigned bits; char bytes[4]; };
int dispatch();
int out_of_line(int);
}
