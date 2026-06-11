import os
import subprocess

def replace_in_file(filepath):
    try:
        with open(filepath, 'r', encoding='utf-8') as f:
            content = f.read()
    except Exception as e:
        print(f"Skipping {filepath}: {e}")
        return

    new_content = content.replace('dpu_', 'itoken_') \
                         .replace('dpu-', 'itoken-') \
                         .replace('DpuBehaviour', 'ItokenBehaviour') \
                         .replace('DPU', 'iToken') \
                         .replace('Dpu', 'Itoken')

    if new_content != content:
        with open(filepath, 'w', encoding='utf-8') as f:
            f.write(new_content)
        print(f"Updated {filepath}")

# Walk through directories
for root, dirs, files in os.walk('.', topdown=False):
    if '.git' in root or 'target' in root or '.itoken' in root:
        continue

    for file in files:
        if file.endswith('.rs') or file == 'Cargo.toml' or file.endswith('.md'):
            replace_in_file(os.path.join(root, file))

    # Rename files/directories that start with dpu-
    for name in dirs:
        if name.startswith('dpu-'):
            old_path = os.path.join(root, name)
            new_name = name.replace('dpu-', 'itoken-')
            new_path = os.path.join(root, new_name)
            print(f"Renaming directory {old_path} -> {new_path}")
            subprocess.run(['git', 'mv', old_path, new_path])
