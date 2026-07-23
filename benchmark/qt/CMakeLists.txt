cmake_minimum_required(VERSION 3.21)
project(stackhour-bench-qt VERSION 0.1.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)

find_package(Qt6 6.5 REQUIRED COMPONENTS Core Gui Qml Quick Concurrent)
qt_standard_project_setup(REQUIRES 6.5)
if(QT_KNOWN_POLICY_QTP0004)
    qt_policy(SET QTP0004 NEW)
endif()

qt_add_executable(stackhour-bench-qt
    src/main.cpp
    src/backend.cpp
    src/backend.h
)

qt_add_qml_module(stackhour-bench-qt
    URI Stackhour.Bench
    VERSION 1.0
    QML_FILES qml/Main.qml
)

target_link_libraries(stackhour-bench-qt PRIVATE
    Qt6::Core
    Qt6::Gui
    Qt6::Qml
    Qt6::Quick
    Qt6::Concurrent
)
