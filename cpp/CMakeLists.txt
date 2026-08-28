cmake_minimum_required(VERSION 3.20)
project(nanobook_cpp CXX)

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

add_library(nb STATIC src/nb.cpp src/quant.cpp)
target_include_directories(nb PUBLIC include)
target_compile_options(nb PRIVATE -Wall -Wextra -Wpedantic -Wconversion -Wshadow)

add_executable(nb_bench src/bench.cpp)
target_link_libraries(nb_bench PRIVATE nb)
target_compile_options(nb_bench PRIVATE -Wall -Wextra -Wpedantic)

add_executable(nb_test tests/test_nb.cpp)
target_link_libraries(nb_test PRIVATE nb)
target_compile_options(nb_test PRIVATE -Wall -Wextra -Wpedantic)

add_executable(quant_test tests/test_quant.cpp)
target_link_libraries(quant_test PRIVATE nb)
target_compile_options(quant_test PRIVATE -Wall -Wextra -Wpedantic)

enable_testing()
add_test(NAME nb_test COMMAND nb_test)
add_test(NAME quant_test COMMAND quant_test)
