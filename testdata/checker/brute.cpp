#include <iostream>

int main() {
    int n;
    if (!(std::cin >> n)) return 0;
    long long sum = 0;
    for (int i = 0; i < n; ++i) {
        long long value;
        std::cin >> value;
        sum += value;
    }
    std::cout << sum << '\n';
}
