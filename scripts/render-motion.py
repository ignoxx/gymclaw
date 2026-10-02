#!/usr/bin/env python3
"""15-second kinetic GymClaw reel. Offline frames, original synthesized sound."""
import importlib.util
import math
from pathlib import Path
import random
import struct
import wave

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('film', ROOT/'scripts/render-videos.py')
film = importlib.util.module_from_spec(spec)
spec.loader.exec_module(film)
W, H = film.W, film.H
INK, LIME, PAPER = film.INK, film.LIME, film.PAPER


def spring(value):
    value = max(0, min(1, value))
    return 1-math.exp(-7*value)*math.cos(12*value)


def centered(draw, value, y, size, color=INK):
    width = draw.textlength(value, font=film.font(size, True))
    film.text(draw, ((W-width)/2,y), value, size, color, True)


def letter_reveal(image, word, y, size, local, color):
    draw = ImageDraw.Draw(image)
    widths = [draw.textlength(letter,font=film.font(size,True)) for letter in word]
    x = (W-sum(widths))/2
    for index, letter in enumerate(word):
        p = film.ease((local-index*.075)/.6)
        if p > 0:
            layer = Image.new('RGBA',(int(widths[index])+30,size+100))
            ld = ImageDraw.Draw(layer)
            film.text(ld,(0,0),letter,size,color,True)
            layer = layer.rotate((1-p)*-12,resample=Image.Resampling.BICUBIC,expand=True)
            image.paste(layer,(int(x),int(y+(1-p)*340)),layer)
        x += widths[index]


def card(image, x, y, title, detail, angle, color=PAPER, width=330, height=220):
    layer = Image.new('RGBA',(width+30,height+30))
    draw = ImageDraw.Draw(layer)
    film.rounded(draw,(10,15,width+10,height+15),color,20)
    film.text(draw,(32,34),title,52,INK,True)
    film.text(draw,(32,119),detail,25,INK)
    draw.line((32,height-18,width-22,height-18),fill=INK,width=2)
    layer = layer.rotate(angle,resample=Image.Resampling.BICUBIC,expand=True)
    image.paste(layer,(int(x),int(y)),layer)


def motion(t):
    bounds = [0,2.8,5.5,8.2,10.8,13.2,15]
    scene = next((i for i in range(6) if t<bounds[i+1]),5)
    u = t-bounds[scene]
    p = u/(bounds[scene+1]-bounds[scene])
    bg = INK if scene in (1,2,3,4) else LIME if scene==5 else PAPER
    fg = PAPER if bg==INK else INK
    image = Image.new('RGB',(W,H),bg)
    draw = ImageDraw.Draw(image)
    # Coordinate grid slides under each composition, rather than idle dashboard motion.
    grid = '#333a2d' if bg==INK else '#c7d0b4' if bg==LIME else '#e0e3d7'
    shift = int(t*22)%70
    for x in range(-70,W+70,70):
        for y in range(-70,H+70,70):
            draw.ellipse((x+shift,y,x+shift+2,y+2),fill=grid)
    film.text(draw,(70,38),'GYMCLAW / 01',23,fg)
    film.text(draw,(1540,38),'MOTION CONCEPT',20,fg)
    if scene==0:
        letter_reveal(image,'PLAN',100,390,u,INK)
        draw = ImageDraw.Draw(image)
        film.text(draw,(78,530),'YOUR CALENDAR. YOUR WEEK.',26,INK)
        for i,(title,detail) in enumerate([('PUSH','MON 19:45'),('PULL','WED 18:00'),('LEGS','FRI 18:00')]):
            q=spring((u-.4-i*.12)/1.1)
            x=345+i*385+(1-q)*(1000+i*100)
            y=680+math.sin(u*2+i)*10
            card(image,x,y,title,detail,(1-q)*-18+(-4+i*4),LIME if i==0 else PAPER)
        radius=80+80*film.ease(u/2.8)
        draw.arc((W-220-radius,410-radius,W-220+radius,410+radius),-90,-90+360*film.ease(u/2.8),fill=INK,width=4)
    elif scene==1:
        letter_reveal(image,'POLL',80,340,u,PAPER)
        draw=ImageDraw.Draw(image)
        film.text(draw,(90,510),'15 MIN',90,LIME,True)
        film.text(draw,(91,630),'REPORTED CHECK-INS',22,PAPER)
        points=[(580+i*225,825-value*10) for i,value in enumerate([8,12,17,23,18,14])]
        amount=film.ease(u/1.5)*(len(points)-1)
        for i in range(len(points)-1):
            if amount>=i:
                f=min(1,amount-i);a,b=points[i],points[i+1]
                end=(a[0]+(b[0]-a[0])*f,a[1]+(b[1]-a[1])*f)
                draw.line((*a,*end),fill=LIME,width=7)
                draw.ellipse((a[0]-9,a[1]-9,a[0]+9,a[1]+9),fill=LIME)
        film.text(draw,(1470,825),'14',118,PAPER,True)
        # Packet dots travel along an orbit, then land on the stored-count graph.
        for i in range(7):
            angle=u*2+i*math.tau/7
            x=950+math.cos(angle)*150;y=735+math.sin(angle)*150
            r=3+i%3
            draw.ellipse((x-r,y-r,x+r,y+r),fill='#91a95f')
    elif scene==2:
        letter_reveal(image,'TRAIN',80,345,u,LIME)
        draw=ImageDraw.Draw(image)
        film.text(draw,(90,585),'80 × 9',137,PAPER,True)
        film.text(draw,(94,760),'SEND YOUR SET.',27,PAPER)
        size=int(470+80*spring(u/1.3))
        image.paste(film.pose('bench-press',size),(int(1280-size/2),int(690-size/2)))
        draw=ImageDraw.Draw(image)
        for index in range(3):
            radius=290+index*32;angle=u*45+index*110
            draw.arc((1280-radius,690-radius,1280+radius,690+radius),angle,angle+95,fill=LIME if index==0 else '#596447',width=3 if index else 6)
    elif scene==3:
        radius=int(350+35*math.sin(p*math.pi))
        cx,cy=960,600
        draw.ellipse((cx-radius,cy-radius,cx+radius,cy+radius),outline='#48503e',width=22)
        draw.arc((cx-radius,cy-radius,cx+radius,cy+radius),-90,-90+360*(1-p),fill=LIME,width=24)
        centered(draw,str(max(0,int(90*(1-p)))),385,280,PAPER)
        centered(draw,'REST',690,90,LIME)
        film.text(draw,(82,245),'SET SAVED',31,PAPER)
        film.text(draw,(1490,850),'NEXT SET READY',25,PAPER)
        # Tick marks converge toward the timer perimeter.
        for i in range(36):
            a=i*math.tau/36+t*.08
            outer=radius+37;inner=outer-10
            draw.line((cx+math.cos(a)*inner,cy+math.sin(a)*inner,cx+math.cos(a)*outer,cy+math.sin(a)*outer),fill='#788367',width=2)
    elif scene==4:
        letter_reveal(image,'NEXT',30,355,u,LIME)
        draw=ImageDraw.Draw(image)
        film.text(draw,(90,487),'EXERCISE IMAGE / TELEGRAM',27,PAPER)
        for i,(slug,name) in enumerate([('bench-press','BENCH PRESS'),('overhead-press','OVERHEAD PRESS'),('tricep-pushdown','TRICEPS')]):
            q=spring((u-.2-i*.1)/1.1)
            layer=Image.new('RGBA',(400,445),INK)
            ld=ImageDraw.Draw(layer)
            film.rounded(ld,(0,0,398,443),'#30382a',18,outline='#667852',width=2)
            layer.paste(film.pose(slug,320),(40,30))
            film.text(ld,(25,375),name,32,LIME,True)
            rotated=layer.rotate((-6+i*6)*(1-p),resample=Image.Resampling.BICUBIC,expand=True)
            x=285+i*440+(1-q)*900
            image.paste(rotated,(int(x),int(580+(1-q)*150)),rotated)
    else:
        q=spring(u/.9)
        size=int(420+(1-q)*180)
        centered(draw,'gymclaw.',250,size,INK)
        centered(draw,'OpenClaw + NemoClaw',780,52,INK)
        draw.line((380,740,380+1160*film.ease(u/.8),740),fill=INK,width=3)
        film.text(draw,(460,934),'Art: Workout Guide / Bryl Lim / Everkinetic · CC BY-SA 4.0',22,INK)
        film.text(draw,(530,974),'Dark PNG background added. Source and license in videos/README.md.',18,INK)
    # A fast geometric wipe ties cuts together without full-frame strobe effects.
    if p>.88 and scene<5:
        q=film.ease((p-.88)/.12)
        x=int(W*(1-q))
        draw.polygon([(x,0),(W+400,0),(W+400,H),(x-300,H)],fill=LIME if scene%2==0 else PAPER)
    return image


def soundtrack(path):
    rate=48000;rng=random.Random(17)
    with wave.open(str(path),'wb') as output:
        output.setnchannels(1);output.setsampwidth(2);output.setframerate(rate)
        frames=bytearray()
        for n in range(15*rate):
            t=n/rate;beat=t%.5;hat=t%.25
            kick=.23*math.exp(-24*beat)*math.sin(math.tau*(52*beat+2*(1-math.exp(-30*beat))))
            tick=.04*math.exp(-110*hat)*(rng.random()*2-1)
            pad=.025*(math.sin(math.tau*220*t)+math.sin(math.tau*277.18*t)+math.sin(math.tau*329.63*t))
            envelope=min(1,t/.1,(15-t)/.4)
            sample=max(-1,min(1,(kick+tick+pad)*envelope))
            frames.extend(struct.pack('<h',int(sample*32767)))
        output.writeframes(frames)


if __name__=='__main__':
    path=Path('/tmp/gymclaw-motion-sound.wav')
    soundtrack(path)
    film.render(ROOT/'videos/gymclaw-motion-15s.mp4',15,motion,path)
