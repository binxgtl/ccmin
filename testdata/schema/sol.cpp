// Sum of the array.
//
// Constructed to exhibit two disagreements with the reference:
//   * values >= 500000 are summed wrongly -- the real bug, reachable by any
//     input the statement permits;
//   * a value of 0 is also summed wrongly -- unreachable, because the
//     statement guarantees 1 <= a_i.
//
// A reducer that does not know the constraints will find the second one,
// because 0 is where unconstrained value shrinking heads, and report a
// counterexample the judge could never produce.
#include <iostream>
#include <vector>

int main() {
    int n;
    if (!(std::cin >> n)) return 0;
    std::vector<long long> a(n);
    for (int i = 0; i < n; ++i) std::cin >> a[i];

    long long sum = 0;
    for (long long x : a) {
        if (x == 0) {
            sum += 1;             // outside the stated constraints
        } else if (x >= 500000) {
            sum += x + 1;         // the real bug
        } else {
            sum += x;
        }
    }
    std::cout << sum << "\n";
    return 0;
}
