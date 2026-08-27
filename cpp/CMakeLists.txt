cmake_minimum_required(VERSION 3.20)
project(latency_ladder_cpp CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

if(NOT CMAKE_BUILD_TYPE)
  set(CMAKE_BUILD_TYPE Release)
endif()

# Matched to the Rust profile as closely as the two toolchains allow, so the
# comparison is between the code and not between two sets of compiler flags.
# Rust uses opt-level=3, lto="fat", codegen-units=1.
set(CMAKE_CXX_FLAGS_RELEASE "-O3 -DNDEBUG")
set(CMAKE_INTERPROCEDURAL_OPTIMIZATION TRUE)

add_library(ll STATIC src/ll.cpp src/quant.cpp)
target_include_directories(ll PUBLIC include)
target_compile_options(ll PRIVATE -Wall -Wextra -Wpedantic -Wconversion -Wshadow)

add_executable(ll_bench src/bench.cpp)
target_link_libraries(ll_bench PRIVATE ll)
target_compile_options(ll_bench PRIVATE -Wall -Wextra -Wpedantic)

add_executable(ll_test tests/test_ll.cpp)
target_link_libraries(ll_test PRIVATE ll)
target_compile_options(ll_test PRIVATE -Wall -Wextra -Wpedantic)

add_executable(quant_test tests/test_quant.cpp)
target_link_libraries(quant_test PRIVATE ll)
target_compile_options(quant_test PRIVATE -Wall -Wextra -Wpedantic)

enable_testing()
add_test(NAME ll_test COMMAND ll_test)
add_test(NAME quant_test COMMAND quant_test)
