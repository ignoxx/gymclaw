#!/usr/bin/env python3
"""Render synthetic demo films offline with Pillow and ffmpeg. No live agent calls."""
import argparse
from functools import lru_cache
import json
import math
from pathlib import Path
import subprocess

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[1]
W, H, FPS = 1920, 1080, 30
PAPER = '#f1f1eb'
INK = '#242824'
LIME = '#d7f580'
MUTED = '#7b8376'


@lru_cache(maxsize=128)
def font(size, display=False):
    name = 'BarlowCondensed-SemiBold.ttf' if display else 'DM-Sans.ttf'
    return ImageFont.truetype(str(ROOT / 'gymclaw/web/fonts' / name), size)


def text(draw, xy, value, size=28, fill=INK, display=False):
    draw.text(xy, str(value), font=font(size, display), fill=fill, spacing=8)


def ease(value):
    value = max(0, min(1, value))
    return 1-(1-value)**3


@lru_cache(maxsize=40)
def pose(slug, size):
    with Image.open(ROOT / 'gymclaw/assets/workout-guide' / slug / 'frame-1.png') as original:
        return original.convert('RGB').resize((size,size),Image.Resampling.LANCZOS)


def rounded(draw, box, fill, radius=22, outline=None, width=1):
    draw.rounded_rectangle(box,radius,fill=fill,outline=outline,width=width)


def wrapped(draw, xy, value, width, size=28, fill=INK):
    lines=[]
    for paragraph in value.split('\n'):
        line=''
        for word in paragraph.split():
            candidate=(line+' '+word).strip()
            if draw.textlength(candidate,font=font(size))>width and line:
                lines.append(line);line=word
            else:line=candidate
        lines.append(line)
    text(draw,xy,'\n'.join(lines),size,fill)
    return len(lines)*(size+8)


def phone_base(image, clock, lock=False):
    draw=ImageDraw.Draw(image)
    rounded(draw,(1145,50,1715,1035),'#d6dbcf',52)
    rounded(draw,(1157,42,1727,1027),INK,52)
    rounded(draw,(1169,55,1715,1012),'#e8eddf' if not lock else '#dce4d2',42)
    rounded(draw,(1330,64,1550,91),INK,16)
    text(draw,(1200,100),clock,22)
    text(draw,(1610,100),'●',19)
    if not lock:
        draw.line((1180,152,1705,152),fill='#c4ceba',width=1)
        draw.ellipse((1200,172,1248,220),fill=INK)
        text(draw,(1214,173),'G',27,LIME,True)
        text(draw,(1265,170),'GymClaw',26)
        text(draw,(1265,205),'Telegram demo',17,MUTED)
    return draw


def bubble(draw, y, body, owner=False, size=26):
    x=1300 if owner else 1200
    width=375 if owner else 472
    lines=[]
    for paragraph in body.split('\n'):
        line=''
        for word in paragraph.split():
            candidate=(line+' '+word).strip()
            if draw.textlength(candidate,font=font(size))>width-36 and line:
                lines.append(line);line=word
            else:line=candidate
        lines.append(line)
    height=len(lines)*(size+8)+28
    rounded(draw,(x,y,x+width,y+height),INK if owner else '#fcfcf8',16)
    text(draw,(x+18,y+12),'\n'.join(lines),size,'#ffffff' if owner else INK)
    return y+height+18


def picture_bubble(image,y,slug,body):
    draw=ImageDraw.Draw(image)
    rounded(draw,(1200,y,1672,y+376),'#fcfcf8',18)
    image.paste(pose(slug,250),(1310,y+10))
    wrapped(draw,(1218,y+276),body,435,24)
    return y+394


def buttons(draw,y,labels,selected=None):
    n=len(labels);width=(472-(n-1)*8)//n
    for index,label in enumerate(labels):
        x=1200+index*(width+8)
        rounded(draw,(x,y,x+width,y+48),LIME if label==selected else '#d3dfc6',10)
        size=18 if n>2 else 22
        tw=draw.textlength(label,font=font(size))
        text(draw,(x+(width-tw)/2,y+12),label,size)


def shell(title, subtitle, number, progress, clock, lock=False):
    image=Image.new('RGB',(W,H),PAPER);draw=ImageDraw.Draw(image)
    text(draw,(100,65),'gymclaw.',44,display=True)
    text(draw,(1370,17),'Demo · simulated Telegram + time',18,MUTED)
    text(draw,(100,185),f'{number:02d}',24,MUTED)
    text(draw,(100,242),title,112,display=True)
    wrapped(draw,(103,555),subtitle,825,31,MUTED)
    draw.line((100,973,970,973),fill='#d9dfcf',width=3)
    draw.line((100,973,100+870*progress,973),fill=INK,width=3)
    text(draw,(100,1004),'OpenClaw / NemoClaw · SQLite',22,MUTED)
    phone_base(image,clock,lock)
    return image


def walkthrough(t, story):
    boundaries=[0,9,17,25,34,44,52,60,69,78]
    index=next((i for i in range(len(boundaries)-1) if t<boundaries[i+1]),8)
    local=t-boundaries[index];span=boundaries[index+1]-boundaries[index];p=local/span
    titles=['Your week\nis ready.','Time to\nget ready.','Check the\ncrowd.','Start with\na warm-up.','Log a set.\nRest begins.','Rest over.\nKeep going.','Next\nexercise.','Machine busy?\nChange order.','Workout\nsaved.']
    subtitles=['Sunday, 07:15. A short brief, before you open your calendar.',
        f"Monday, {story['ready_time']}. Prep and travel buffers are part of the plan.",
        f"Expected comfort {story['estimate']}, low confidence. Synthetic counts and arrival labels.",
        'An exercise image and the weight to start with.',
        'Weights and reps persist before the rest intent is queued.',
        '90 seconds pass in simulated time. The next set is ready.',
        'Finish the exercise. The next one arrives with its illustration.',
        'Reorder first. Choose an approved alternative when needed.',
        f"{story['completed_sets']} working sets saved. Next workout already planned."]
    clock='07:15' if index==0 else story['ready_time'] if index==1 else story['arrival_time']
    image=shell(titles[index],subtitles[index],index+1,t/78,clock,index<2)
    draw=ImageDraw.Draw(image)
    if index<2:
        text(draw,(1280,235),clock,100,display=True)
        text(draw,(1310,355),'Sunday' if index==0 else 'Monday',25,MUTED)
        y=465+int((1-ease(local/1.2))*70)
        rounded(draw,(1200,y,1672,y+265),'#fcfcf8',20)
        text(draw,(1220,y+18),'GymClaw',24)
        if index==0:
            body='\n'.join(f"{s['day']}  {s['name']}  {s['time']}" for s in story['week'])
            text(draw,(1220,y+65),body,25)
            text(draw,(1220,y+190),f"Crowd {story['estimate']} · low confidence",20,MUTED)
        else:
            wrapped(draw,(1220,y+65),f"Gym at {story['arrival_time']}.\nGet ready now.\nLeave at {story['leave_time']}.",425,28)
    elif index==2:
        y=bubble(draw,260,f"Crowd estimate {story['estimate']}.\nHow is it on arrival?")
        buttons(draw,y,['Quiet','Fine','Busy'],'Fine' if p>.38 else None)
        if p>.48:bubble(draw,y+77,'Fine. Start Push.',True)
        text(draw,(1200,810),'Arrival feedback calibrates your estimate.',20,MUTED)
    elif index==3:
        y=picture_bubble(image,255,'bench-press','Bench press\nWarm-up 50 kg × 8')
        if p>.42:bubble(draw,y,'50 × 8, warm-up',True)
        if p>.75:bubble(draw,y+95,'Next 80 kg × 8–10.',False,23)
    elif index==4:
        y=picture_bubble(image,250,'bench-press','Bench press\n80 kg × 8–10 · set 1 / 3')
        if p>.2:y=bubble(draw,y,'80 × 9',True)
        if p>.45:
            y=bubble(draw,y,'Rest 90s.',False)
            remaining=max(0,int(90*(1-(p-.45)/.55)))
            rounded(draw,(1220,y,1650,y+97),LIME,18)
            text(draw,(1340,y+8),f'{remaining}s',58,display=True)
    elif index==5:
        y=bubble(draw,270,'Rest over.\nBench press · set 2 / 3\n80 kg × 8–10.')
        if p>.25:y=bubble(draw,y,'80 × 10',True)
        if p>.55:y=bubble(draw,y,'One more set after rest.')
        if p>.8:bubble(draw,y,'80 × 10',True)
    elif index==6:
        slug=story['next']['illustration']['guide_id']
        y=picture_bubble(image,255,slug,f"{story['next']['name']}\n{story['next']['target_weight']:g} kg × 8–10")
        buttons(draw,y,['Occupied','Alternative','Skip'])
    elif index==7:
        if p<.35:
            y=picture_bubble(image,255,story['next']['illustration']['guide_id'],story['next']['name'])
            buttons(draw,y,['Occupied','Alternative','Skip'],'Occupied')
        else:
            card=story['busy']
            y=picture_bubble(image,255,card['illustration']['guide_id'],f"{card['name']}\n{card['target_weight']:g} kg × {card['rep_min']}–{card['rep_max']}")
            bubble(draw,y,'Do this now. Return to press next.',False,23)
    else:
        y=bubble(draw,275,f"Push complete.\n{story['completed_sets']} working sets + warm-up saved.")
        y=bubble(draw,y,f"Next: {story['week'][1]['name']}\n{story['week'][1]['day']} {story['week'][1]['time']}")
        rounded(draw,(1210,y+20,1660,y+145),LIME,20)
        text(draw,(1270,y+55),'Progress saved.',39,display=True)
        text(draw,(105,705),'Art: Workout Guide / Bryl Lim / Everkinetic',21,MUTED)
        text(draw,(105,743),'CC BY-SA 4.0 · dark PNG background added',21,MUTED)
        text(draw,(105,781),'github.com/bryllim/workout-guide',18,MUTED)
        text(draw,(105,815),'creativecommons.org/licenses/by-sa/4.0/',18,MUTED)
    return image


def render(output, duration, frame, audio=None):
    output.parent.mkdir(parents=True,exist_ok=True)
    argv=['ffmpeg','-y','-loglevel','error','-f','rawvideo','-pix_fmt','rgb24','-s',f'{W}x{H}','-r',str(FPS),'-i','-']
    if audio:argv+=['-i',str(audio)]
    argv+=['-c:v','libx264','-preset','veryfast','-crf','20','-pix_fmt','yuv420p','-movflags','+faststart']
    if audio:argv+=['-c:a','aac','-b:a','128k','-af','apad','-t',str(duration)]
    else:argv+=['-t',str(duration)]
    argv += [str(output)]
    process=subprocess.Popen(argv,stdin=subprocess.PIPE)
    try:
        for n in range(int(duration*FPS)):
            process.stdin.write(frame(n/FPS).tobytes())
            if n%300==0:print(f'{n/FPS:.0f}s / {duration}s',flush=True)
        process.stdin.close()
        if process.wait()!=0:raise RuntimeError('ffmpeg failed')
    except BaseException:
        process.terminate();process.wait();raise


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--story',type=Path,required=True)
    parser.add_argument('--output',type=Path,default=ROOT/'videos/gymclaw-walkthrough.mp4')
    parser.add_argument('--audio',type=Path)
    args=parser.parse_args()
    story=json.loads(args.story.read_text())
    assert story['demo'] is True and story['status']=='PLAN_UPDATED'
    render(args.output,78,lambda t:walkthrough(t,story),args.audio)


if __name__=='__main__':main()
