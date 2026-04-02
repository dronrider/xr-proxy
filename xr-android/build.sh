#!/bin/bash
export JAVA_HOME=/home/rider/android-tools/jdk17
export ANDROID_HOME=/home/rider/android-tools/sdk
export PATH="$JAVA_HOME/bin:$PATH"
cd "$(dirname "$0")"
sh ./gradlew assembleDebug --no-daemon 2>&1
