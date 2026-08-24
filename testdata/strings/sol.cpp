// Count how many words equal "a". Reads until EOF.
#include <iostream>
#include <string>
#include <algorithm>

int main() {
    std::string w;
    int count = 0;
    while (std::cin >> w) {
        std::string lower = w;
        std::transform(lower.begin(), lower.end(), lower.begin(), ::tolower);
        if (lower == "a") ++count;   // BUG: should be case-sensitive
    }
    std::cout << count << "\n";
    return 0;
}
