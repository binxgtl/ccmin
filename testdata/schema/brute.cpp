// Reference: just add them up.
#include <iostream>
#include <vector>

int main() {
    int n;
    if (!(std::cin >> n)) return 0;
    std::vector<long long> a(n);
    for (int i = 0; i < n; ++i) std::cin >> a[i];

    long long sum = 0;
    for (long long x : a) sum += x;
    std::cout << sum << "\n";
    return 0;
}
